//! 进程内 LRU + TTL 响应缓存。
//!
//! 目标：对"非流" OpenAI 兼容 chat completion 请求做精确缓存，命中后直接返回上游响应，
//! 省去对上游 API 的重复调用，特别适合对抗截图里那种短时间内大量相同请求的低级调用场景。
//!
//! 缓存粒度：
//! - key = blake3(`model` + `messages` + 关键采样参数 + 工具调用相关字段) 截断为 u64
//! - 排除 `user`（隐私元数据）与 `stream`（不影响输出内容）
//! - 不同 `temperature`/`top_p`/`seed`/`tools`/`response_format` 等都会形成独立 key
//!
//! 淘汰策略：
//! - 读命中时刷新 LRU 顺序（最近访问放到队尾）
//! - 写入时若超过 `max_entries`，从队首淘汰最久未访问的条目
//! - 命中时若条目已超过 TTL，按 miss 处理并移除
//!
//! 写入过滤：
//! - 仅当**请求体**大小不超过 `max_request_bytes` 时才写入缓存。
//!   请求体越大，占用的内存与后续比对成本越高，因此用它作为缓存占用成本的近似。
//!
//! 预热阈值：
//! - 同一请求（key）+ **同一响应内容**在 `warmup_window` 窗口内累计一致出现
//!   达到 `min_hit_count` 次后，缓存才真正生效（可命中）。
//! - 预热期每次都真实调用上游，通过比对响应的内容指纹来判断是否一致：
//!   与上次一致则计数 +1，不一致则计数重置（重新累计）。
//! - 「一致」只比较模型生成的内容：比对前会剔除 `id`/`created`/`usage` 等
//!   每次调用必变的流水字段，并对 JSON 按键排序（字段顺序不影响结果）。
//! - 只有"短时间内反复出现且响应内容稳定一致"的请求才值得缓存。
//! - 计数表只负责预热期统计：窗口过期后无论是否达标都会删除记录，
//!   正式缓存的生命周期由 TTL/LRU 管理，不依赖计数表。
//! - 未达标时只计数、不写缓存；仅当 `record_occurrence` 返回 `true` 才允许写入。
//! - `min_hit_count == 0` 表示关闭预热（任何请求都可立即命中）。
//!
//! 并发：
//! - 内部 `RwLock`，get/put 操作均为同步内存操作，持锁时间极短，
//!   不会跨越 `.await`，可在 async task 中安全使用。

use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// 缓存键：blake3 截断为 64 位，碰撞概率极低（百万级条目仍可忽略）。
pub type CacheKey = u64;

/// 单个缓存条目
#[derive(Clone)]
struct CacheEntry {
    /// 完整响应 JSON（与正常返回给客户端的格式一致）
    response: Value,
    /// 入库时间，用于 TTL 过期判断
    stored_at: Instant,
}

impl CacheEntry {
    fn is_expired(&self, ttl: Duration) -> bool {
        self.stored_at.elapsed() > ttl
    }
}

/// 缓存统计指标
#[derive(Serialize, Clone, Debug)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub evictions: u64,
    pub expirations: u64,
    pub oversize_skips: u64,
    pub current_size: usize,
    pub max_size: usize,
    pub ttl_secs: u64,
    /// 当前仍处于预热观察期（未达阈值）的 key 数量
    pub warming_keys: usize,
    /// 预热阈值（0 表示关闭预热）
    pub min_hit_count: u32,
}

impl CacheStats {
    /// 命中率（0.0 ~ 1.0）
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

/// 预热状态：记录某 key 在窗口内的"一致响应"累计情况
struct Occurrence {
    /// 连续一致的响应计数
    count: u32,
    /// 窗口起点
    first_seen: Instant,
    /// 最近一次响应的内容指纹（用于比对是否一致）
    response_hash: u64,
    /// 是否已达到预热阈值（此后该 key 可命中缓存）
    warmed_up: bool,
}

/// 缓存内部状态
struct CacheInner {
    entries: HashMap<CacheKey, CacheEntry>,
    /// LRU 顺序：front = 最久未访问，back = 最近访问/写入
    order: VecDeque<CacheKey>,
    /// 预热状态：key -> 出现计数/窗口/指纹/是否达标
    occurrences: HashMap<CacheKey, Occurrence>,
    hits: u64,
    misses: u64,
    stores: u64,
    evictions: u64,
    expirations: u64,
    oversize_skips: u64,
}

/// TTL + LRU 响应缓存（可选预热阈值）
pub struct ResponseCache {
    inner: RwLock<CacheInner>,
    ttl: Duration,
    max_entries: usize,
    /// 单个请求体 JSON 的最大字节数；请求体超过此值时，对应响应直接不入缓存
    max_request_bytes: usize,
    /// 预热阈值：窗口内一致响应达到该次数才允许命中；0 = 关闭预热（立即生效）
    min_hit_count: u32,
    /// 预热窗口时长
    warmup_window: Duration,
}

impl ResponseCache {
    /// 创建缓存。默认关闭预热（`min_hit_count = 0`），如需开启请链式调用 [`ResponseCache::with_warmup`]。
    pub fn new(ttl_secs: u64, max_entries: usize, max_request_bytes: usize) -> Self {
        Self {
            inner: RwLock::new(CacheInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                occurrences: HashMap::new(),
                hits: 0,
                misses: 0,
                stores: 0,
                evictions: 0,
                expirations: 0,
                oversize_skips: 0,
            }),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
            max_request_bytes,
            min_hit_count: 0,
            warmup_window: Duration::ZERO,
        }
    }

    /// 开启预热阈值：同一请求（key）且**同一响应内容**在 `window` 内累计一致出现
    /// `min_hit_count` 次后才允许命中缓存。
    pub fn with_warmup(mut self, min_hit_count: u32, window: Duration) -> Self {
        self.min_hit_count = min_hit_count;
        self.warmup_window = window;
        self
    }

    /// 查询某 key 是否已完成预热，允许命中缓存。
    ///
    /// 预热关闭时恒为 `true`。预热开启时仅当该 key 在窗口内已累计到
    /// `min_hit_count` 次一致的响应才为 `true`。
    pub fn is_warmed_up(&self, key: CacheKey) -> bool {
        if self.min_hit_count == 0 {
            return true;
        }
        let inner = self.inner.read().unwrap();
        // 正式缓存中已有未过期条目 → 可命中。
        // 计数表只负责预热期统计、窗口过期即删；正式缓存的生命周期由 TTL/LRU 管理，
        // 因此即使计数表记录已被清理，只要正式缓存条目仍有效即可继续命中。
        if let Some(entry) = inner.entries.get(&key) {
            if !entry.is_expired(self.ttl) {
                return true;
            }
        }
        match inner.occurrences.get(&key) {
            Some(o) => o.warmed_up,
            None => false,
        }
    }

    /// 记录一次上游响应，用于预热累计。返回该 key 本次是否已达到阈值（应写入缓存）。
    ///
    /// 预热规则（同一 key 下）：
    /// - 响应内容与窗口内上次一致 → 计数 +1；不一致 → 计数重置为 1（重新累计）。
    /// - 计数达到 `min_hit_count` → 标记 `warmed_up` 并返回 `true`。
    /// - 窗口过期 → 以本次响应重新开始累计。
    /// - 预热关闭 → 恒返回 `true`（无预热语义）。
    /// 记录一次上游响应，用于预热累计。返回 `true` 表示本次已达到阈值，
    /// **调用方必须仅在返回 `true` 时才允许把响应写入正式缓存**（未达标只计数、不写缓存）。
    ///
    /// 预热规则（同一 key 下）：
    /// - 响应内容与窗口内上次一致 → 计数 +1；不一致 → 计数重置为 1（重新累计）。
    ///   「一致」只比较模型生成的内容，`id`/`created`/`usage` 等每次必变的流水字段会被剔除。
    /// - 计数达到 `min_hit_count` → 标记 `warmed_up` 并返回 `true`。
    /// - 窗口过期 → 删除计数表记录，以本次响应重新开始累计（无论是否达标都不保留）。
    /// - 预热关闭 → 恒返回 `true`（无预热语义）。
    pub fn record_occurrence(&self, key: CacheKey, response: &Value) -> bool {
        // 预热关闭：无需统计
        if self.min_hit_count == 0 {
            return true;
        }

        // 一致性比对只针对"模型生成的内容"：剔除 id/created/usage 等流水字段，
        // 并对 JSON 按键排序，保证字段顺序不影响结果。
        let resp_hash = hash_value(response);
        let mut inner = self.inner.write().unwrap();
        let now = Instant::now();

        // 正式缓存已有未过期条目：早已达标并入库，直接视为达标（幂等）
        if let Some(entry) = inner.entries.get(&key) {
            if !entry.is_expired(self.ttl) {
                return true;
            }
            // 过期条目直接移除，交给预热流程重新累计
            inner.entries.remove(&key);
            inner.order.retain(|k| *k != key);
            inner.expirations += 1;
        }

        // 清理规则：只要过了预热窗口，无论是否达标都从计数表删除。
        // 计数表只负责预热期统计，正式缓存的生命周期由 TTL/LRU 管理。
        if let Some(o) = inner.occurrences.get(&key) {
            if now.duration_since(o.first_seen) > self.warmup_window {
                inner.occurrences.remove(&key);
            }
        }

        // 容量保护：达到上限时清理所有窗口过期的条目，避免计数表无限增长
        if inner.occurrences.len() >= self.max_entries * 2 {
            inner.occurrences.retain(|_, o| {
                now.duration_since(o.first_seen) <= self.warmup_window
            });
            if inner.occurrences.len() >= self.max_entries * 2 {
                return false;
            }
        }

        let entry = inner.occurrences.entry(key).or_insert_with(|| Occurrence {
            count: 0,
            first_seen: now,
            response_hash: 0,
            warmed_up: false,
        });

        // 已达标：保持达标状态（幂等），响应变更时也视为达标继续命中
        if entry.warmed_up {
            return true;
        }

        // 窗口过期：以本次响应重新开始累计
        if now.duration_since(entry.first_seen) > self.warmup_window {
            entry.count = 0;
            entry.first_seen = now;
            entry.response_hash = 0;
        }

        if entry.count == 0 || entry.response_hash == resp_hash {
            // 首次或响应一致：计数 +1
            entry.count += 1;
            entry.response_hash = resp_hash;
        } else {
            // 响应不一致：重置计数，以本次响应重新累计
            entry.count = 1;
            entry.response_hash = resp_hash;
            entry.first_seen = now;
        }

        if entry.count >= self.min_hit_count {
            entry.warmed_up = true;
            true
        } else {
            false
        }
    }

    /// 查询缓存。命中时刷新 LRU 顺序并返回克隆的响应 Value。
    pub fn get(&self, key: CacheKey) -> Option<Value> {
        let mut inner = self.inner.write().unwrap();

        // 先把要返回的数据以及是否过期提取出来，及时释放对 entries 的不可变借用
        let (response, expired) = match inner.entries.get(&key) {
            Some(entry) => (Some(entry.response.clone()), entry.is_expired(self.ttl)),
            None => (None, false),
        };

        match (response, expired) {
            (Some(resp), false) => {
                // 命中：把 key 挪到队尾（最近访问）
                inner.order.retain(|k| *k != key);
                inner.order.push_back(key);
                inner.hits += 1;
                Some(resp)
            }
            (Some(_), true) => {
                // 已过期：移除并按 miss 处理
                inner.entries.remove(&key);
                inner.order.retain(|k| *k != key);
                inner.expirations += 1;
                inner.misses += 1;
                None
            }
            (None, _) => {
                inner.misses += 1;
                None
            }
        }
    }

    /// 写入缓存。请求体超过大小上限时直接跳过（不入缓存）。
    ///
    /// `request_bytes` 为发起请求的原始请求体字节数。
    pub fn put(&self, key: CacheKey, response: Value, request_bytes: usize) {
        let mut inner = self.inner.write().unwrap();

        // 大小检查：请求体超过上限则不入缓存
        if request_bytes > self.max_request_bytes {
            inner.oversize_skips += 1;
            return;
        }

        // 已有相同 key：更新值并把 key 挪到队尾
        if inner.entries.contains_key(&key) {
            inner.order.retain(|k| *k != key);
            inner.entries.insert(
                key,
                CacheEntry {
                    response,
                    stored_at: Instant::now(),
                },
            );
            inner.order.push_back(key);
            return;
        }

        // 容量已满：淘汰最久未访问条目，直到腾出空间
        while inner.entries.len() >= self.max_entries {
            if let Some(oldest) = inner.order.pop_front() {
                inner.entries.remove(&oldest);
                inner.evictions += 1;
            } else {
                break;
            }
        }

        inner.entries.insert(
            key,
            CacheEntry {
                response,
                stored_at: Instant::now(),
            },
        );
        inner.order.push_back(key);
        inner.stores += 1;
    }

    /// 拷贝一份缓存统计快照
    pub fn stats(&self) -> CacheStats {
        let inner = self.inner.read().unwrap();
        let warming_keys = if self.min_hit_count == 0 {
            0
        } else {
            inner
                .occurrences
                .values()
                .filter(|o| !o.warmed_up)
                .count()
        };
        CacheStats {
            hits: inner.hits,
            misses: inner.misses,
            stores: inner.stores,
            evictions: inner.evictions,
            expirations: inner.expirations,
            oversize_skips: inner.oversize_skips,
            current_size: inner.entries.len(),
            max_size: self.max_entries,
            ttl_secs: self.ttl.as_secs(),
            warming_keys,
            min_hit_count: self.min_hit_count,
        }
    }

    /// 清空缓存（一般用于运维/调试）
    #[allow(dead_code)]
    pub fn clear(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.entries.clear();
        inner.order.clear();
        inner.occurrences.clear();
    }
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 计算响应内容的 64 位指纹，用于预热时比对"同一请求是否得到同一响应"。
///
/// 比对前先做规范化：剔除每次调用必变的流水字段（id/时间/usage 等），
/// 并对 JSON 按键排序，保证只要模型生成的内容一致就视为同一响应。
fn hash_value(value: &Value) -> u64 {
    let mut canonical = value.clone();
    canonicalize_response(&mut canonical);
    let hash = blake3::hash(serde_json::to_string(&canonical).unwrap_or_default().as_bytes());
    let bytes = hash.as_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

/// 规范化响应，仅用于"一致性比对"（不用于存储）：
/// 1. 剔除每次调用必变的流水字段：请求/响应 `id`、时间戳、token 用量等；
/// 2. 对 JSON 字段递归按键排序，保证字段顺序不影响比对结果。
fn canonicalize_response(value: &mut Value) {
    if let Some(obj) = value.as_object_mut() {
        // 顶层流水字段（随每次调用必然变化）
        for f in [
            "id",
            "object",
            "created",
            "usage",
            "system_fingerprint",
            "service_tier",
            "request_id",
        ] {
            obj.remove(f);
        }
        // choices 内部嵌套的流水字段
        if let Some(choices) = obj.get_mut("choices") {
            if let Some(arr) = choices.as_array_mut() {
                for choice in arr {
                    canonicalize_choice(choice);
                }
            }
        }
    }
    sort_object_keys(value);
}

/// 剔除单个 choice 中的流水字段（choice 级 `id`、message/delta 的 `id`、tool_call 的 `id`）。
fn canonicalize_choice(choice: &mut Value) {
    if let Some(obj) = choice.as_object_mut() {
        obj.remove("id");
        for slot in ["message", "delta"] {
            if let Some(msg) = obj.get_mut(slot) {
                if let Some(mo) = msg.as_object_mut() {
                    mo.remove("id");
                    // tool_call 的 id 每次调用必变，剔除后只保留 name/arguments
                    if let Some(tcs) = mo.get_mut("tool_calls") {
                        if let Some(tc_arr) = tcs.as_array_mut() {
                            for tc in tc_arr {
                                if let Some(tc_obj) = tc.as_object_mut() {
                                    tc_obj.remove("id");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// 递归按键排序，保证 JSON 字段顺序不影响比对结果。
/// （serde_json 默认 Map 已是 BTreeMap 排序；`sort_keys()` 在开启
///   `preserve_order` 特性时也会按键重排，规避字段顺序敏感的问题。）
fn sort_object_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.sort_keys();
            for v in map.values_mut() {
                sort_object_keys(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                sort_object_keys(v);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 缓存键计算
// ---------------------------------------------------------------------------

/// 必须出现在 key 中的字段（缺失或空时本请求不入缓存）
const REQUIRED_FIELDS: &[&str] = &["model", "messages"];

/// 影响输出的可选字段（出现时纳入 key 计算）
const OPTIONAL_FIELDS: &[&str] = &[
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "seed",
    "stop",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "response_format",
    "logit_bias",
    "max_tokens",
    "max_completion_tokens",
    "logprobs",
    "n",
    "stream_options",
];

/// 计算缓存键。
///
/// 仅对"影响输出"的字段做哈希：
/// - 必含：`model` + `messages`
/// - 可选：上述采样参数/工具调用等
///
/// 显式忽略：`user`（隐私元数据）、`stream`（仅影响响应包装形态，不影响内容）。
///
/// 返回 `None` 表示该请求不可缓存（如缺 model 或 messages 为空数组）。
pub fn compute_cache_key(data: &Value) -> Option<CacheKey> {
    if !data.is_object() {
        return None;
    }

    let obj = data.as_object().unwrap();

    // 必含字段检查
    for field in REQUIRED_FIELDS {
        match obj.get(*field) {
            Some(v) if !v.is_null() => {
                if let Some(arr) = v.as_array() {
                    if arr.is_empty() {
                        return None;
                    }
                }
            }
            _ => return None,
        }
    }

    // 构造用于哈希的子对象（BTreeMap 语义，自动按 key 排序，序列化结果稳定）
    let mut key_obj = serde_json::Map::new();
    for field in REQUIRED_FIELDS.iter().chain(OPTIONAL_FIELDS.iter()) {
        if let Some(v) = obj.get(*field) {
            if v.is_null() {
                continue;
            }
            // 跳过空数组（与"未设置"等价）
            if let Some(arr) = v.as_array() {
                if arr.is_empty() {
                    continue;
                }
            }
            key_obj.insert((*field).to_string(), v.clone());
        }
    }

    // serde_json 默认按 BTreeMap 排序，序列化结果即 canonical JSON
    let canonical = serde_json::to_string(&Value::Object(key_obj)).ok()?;

    let hash = blake3::hash(canonical.as_bytes());
    let bytes = hash.as_bytes();
    // 取前 8 字节构造 u64 key
    Some(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_same_payload_same_key() {
        let a = json!({
            "model": "deepseek-v4-flash",
            "messages": [{"role":"user","content":"hi"}],
            "temperature": 0.7,
            "user": "u1",
        });
        let b = json!({
            // 字段顺序不同也不影响 key
            "user": "u1",
            "messages": [{"role":"user","content":"hi"}],
            "temperature": 0.7,
            "model": "deepseek-v4-flash",
        });
        assert_eq!(compute_cache_key(&a), compute_cache_key(&b));
    }

    #[test]
    fn test_user_field_excluded() {
        let a = json!({
            "model": "m",
            "messages": [{"role":"user","content":"hi"}],
            "user": "u1",
        });
        let b = json!({
            "model": "m",
            "messages": [{"role":"user","content":"hi"}],
            "user": "u2",
        });
        assert_eq!(compute_cache_key(&a), compute_cache_key(&b));
    }

    #[test]
    fn test_temperature_changes_key() {
        let a = json!({"model":"m","messages":[{"role":"user","content":"hi"}],"temperature":0.7});
        let b = json!({"model":"m","messages":[{"role":"user","content":"hi"}],"temperature":0.9});
        assert_ne!(compute_cache_key(&a), compute_cache_key(&b));
    }

    #[test]
    fn test_messages_change_key() {
        let a = json!({"model":"m","messages":[{"role":"user","content":"hi"}]});
        let b = json!({"model":"m","messages":[{"role":"user","content":"hello"}]});
        assert_ne!(compute_cache_key(&a), compute_cache_key(&b));
    }

    #[test]
    fn test_missing_model_returns_none() {
        let a = json!({"messages":[{"role":"user","content":"hi"}]});
        assert_eq!(compute_cache_key(&a), None);
    }

    #[test]
    fn test_empty_messages_returns_none() {
        let a = json!({"model":"m","messages":[]});
        assert_eq!(compute_cache_key(&a), None);
    }

    #[test]
    fn test_cache_get_put_hit_evict() {
        let cache = ResponseCache::new(60, 2, 1024);
        let resp1 = json!({"k":1});
        let resp2 = json!({"k":2});
        let resp3 = json!({"k":3});

        cache.put(1, resp1.clone(), 10);
        cache.put(2, resp2.clone(), 10);
        // 命中 key=1 会把它挪到队尾（最新），队首是 key=2
        assert_eq!(cache.get(1), Some(resp1.clone()));
        // 写入 key=3 时淘汰最久未访问的 = key=2
        cache.put(3, resp3.clone(), 10);

        assert_eq!(cache.get(2), None);
        assert_eq!(cache.get(1), Some(resp1));
        assert_eq!(cache.get(3), Some(resp3));

        let stats = cache.stats();
        assert_eq!(stats.evictions, 1);
        assert!(stats.hits >= 2);
    }

    #[test]
    fn test_cache_ttl_expiration() {
        let cache = ResponseCache::new(0, 10, 1024); // TTL = 0，立即过期
        cache.put(1, json!({"k":1}), 10);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(cache.get(1), None);
        assert_eq!(cache.stats().expirations, 1);
    }

    #[test]
    fn test_oversize_skip() {
        let cache = ResponseCache::new(60, 10, 10); // 请求体最多 10 字节
        cache.put(1, json!({"big":"this is definitely way larger than ten bytes"}), 20);
        assert_eq!(cache.get(1), None);
        assert_eq!(cache.stats().oversize_skips, 1);
    }

    #[test]
    fn test_warmup_threshold() {
        // 预热阈值 3 次 / 180s 窗口
        let cache = ResponseCache::new(60, 10, 1024)
            .with_warmup(3, std::time::Duration::from_secs(180));
        let resp = json!({"k":1});
        // 第 1、2 次：未达阈值，不允许命中
        assert!(!cache.record_occurrence(1, &resp));
        assert!(!cache.is_warmed_up(1));
        assert!(!cache.record_occurrence(1, &resp));
        assert!(!cache.is_warmed_up(1));
        // 第 3 次：达到阈值
        assert!(cache.record_occurrence(1, &resp));
        assert!(cache.is_warmed_up(1));
        // 预热关闭时立即生效
        let plain = ResponseCache::new(60, 10, 1024);
        assert!(plain.record_occurrence(2, &resp));
        assert!(plain.is_warmed_up(2));
    }

    #[test]
    fn test_warmup_inconsistent_response_resets() {
        // 响应内容不一致则计数重置，需重新累计
        let cache = ResponseCache::new(60, 10, 1024)
            .with_warmup(3, std::time::Duration::from_secs(180));
        let a = json!({"k":1});
        let b = json!({"k":2});
        assert!(!cache.record_occurrence(1, &a)); // A(1)
        assert!(!cache.record_occurrence(1, &a)); // A(2)
        assert!(!cache.record_occurrence(1, &b)); // B: 不一致，重置为 1
        assert!(!cache.record_occurrence(1, &a)); // A: 又与 B 不一致，重置为 1
        assert!(!cache.record_occurrence(1, &a)); // A(2)
        assert!(cache.record_occurrence(1, &a)); // A(3) 达到阈值
        assert!(cache.is_warmed_up(1));
    }

    #[test]
    fn test_warmup_window_expiry() {
        // 短窗口（50ms）验证窗口过期后计数重置
        let cache = ResponseCache::new(60, 10, 1024)
            .with_warmup(3, std::time::Duration::from_millis(50));
        let resp = json!({"k":1});
        assert!(!cache.record_occurrence(1, &resp));
        std::thread::sleep(std::time::Duration::from_millis(60));
        // 窗口已过期：计数重置，本次仍不允许
        assert!(!cache.record_occurrence(1, &resp));
        assert!(!cache.record_occurrence(1, &resp));
        assert!(cache.record_occurrence(1, &resp));
        // 过期窗口的 key 不再计入 warming
        let stats = cache.stats();
        assert_eq!(stats.warming_keys, 0);
    }

    #[test]
    fn test_warmup_gate_with_cache() {
        // 正确用法：预热未达标时只计数、不写缓存；达标后才写缓存并可命中
        let cache = ResponseCache::new(60, 10, 1024)
            .with_warmup(3, std::time::Duration::from_secs(180));
        let resp = json!({"v":1});
        // 前两次：未达标，返回 false，不写缓存
        assert!(!cache.record_occurrence(9, &resp));
        assert!(cache.get(9).is_none());
        assert!(!cache.record_occurrence(9, &resp));
        assert!(cache.get(9).is_none());
        // 第 3 次：达到阈值，返回 true，随后写缓存 → 可命中
        assert!(cache.record_occurrence(9, &resp));
        cache.put(9, resp.clone(), 10);
        assert!(cache.is_warmed_up(9));
        assert_eq!(cache.get(9), Some(resp));
    }

    #[test]
    fn test_warmup_ignores_dynamic_fields() {
        // 流水字段（id/created/usage）每次必变，但模型生成内容一致 → 应视为一致
        let cache = ResponseCache::new(60, 10, 1024)
            .with_warmup(3, std::time::Duration::from_secs(180));
        let a = json!({
            "id": "chatcmpl-aaa",
            "object": "chat.completion",
            "created": 1000,
            "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "你好"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let b = json!({
            "id": "chatcmpl-bbb",
            "object": "chat.completion",
            "created": 2000,
            "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "你好"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 6, "total_tokens": 17}
        });
        assert_eq!(hash_value(&a), hash_value(&b));
        assert!(!cache.record_occurrence(1, &a)); // 计数 1
        assert!(!cache.record_occurrence(1, &b)); // 仅流水字段不同 → 视为一致，计数 2
        assert!(cache.record_occurrence(1, &a));  // 计数 3 → 达标
        assert!(cache.is_warmed_up(1));
    }

    #[test]
    fn test_warmup_ignores_field_order() {
        // 相同语义内容、不同字段顺序 → 应视为一致
        let cache = ResponseCache::new(60, 10, 1024)
            .with_warmup(3, std::time::Duration::from_secs(180));
        let a = json!({
            "id": "x1",
            "choices": [{"finish_reason": "stop", "message": {"content": "abc", "role": "assistant"}}],
            "usage": {"total_tokens": 1}
        });
        let b = json!({
            "usage": {"total_tokens": 1},
            "choices": [{"message": {"role": "assistant", "content": "abc"}, "finish_reason": "stop"}],
            "id": "x2"
        });
        assert_eq!(hash_value(&a), hash_value(&b));
        assert!(!cache.record_occurrence(2, &a));
        assert!(!cache.record_occurrence(2, &b)); // 字段顺序不同 → 仍视为一致
        assert!(cache.record_occurrence(2, &a));
    }

    #[test]
    fn test_content_difference_still_detected() {
        // 模型生成内容真的不同 → 应判定不一致并重置计数
        let cache = ResponseCache::new(60, 10, 1024)
            .with_warmup(3, std::time::Duration::from_secs(180));
        let a = json!({"choices":[{"message":{"content":"你好"}}]});
        let b = json!({"choices":[{"message":{"content":"再见"}}]});
        assert_ne!(hash_value(&a), hash_value(&b));
        assert!(!cache.record_occurrence(3, &a)); // A(1)
        assert!(!cache.record_occurrence(3, &b)); // 内容不同 → 重置为 1
        assert!(!cache.record_occurrence(3, &a)); // 与上次不同 → 重置为 1
        assert!(!cache.record_occurrence(3, &a)); // A(2)
        assert!(cache.record_occurrence(3, &a));  // A(3)
    }

    #[test]
    fn test_occurrences_cleaned_after_window() {
        // 窗口过期后，无论是否达标，计数表条目都应被清理
        let cache = ResponseCache::new(60, 10, 1024)
            .with_warmup(3, std::time::Duration::from_millis(50));
        let resp = json!({"v":1});
        assert!(!cache.record_occurrence(1, &resp));
        assert!(!cache.record_occurrence(1, &resp));
        assert!(cache.record_occurrence(1, &resp)); // 达标
        assert!(cache.is_warmed_up(1));
        std::thread::sleep(std::time::Duration::from_millis(60));
        // 窗口过期：旧计数记录被清理，重新累计（本次不足阈值）
        assert!(!cache.record_occurrence(1, &resp));
        let stats = cache.stats();
        assert_eq!(stats.warming_keys, 1);
    }
}
