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

/// 缓存内部状态
struct CacheInner {
    entries: HashMap<CacheKey, CacheEntry>,
    /// LRU 顺序：front = 最久未访问，back = 最近访问/写入
    order: VecDeque<CacheKey>,
    hits: u64,
    misses: u64,
    stores: u64,
    evictions: u64,
    expirations: u64,
    oversize_skips: u64,
}

/// TTL + LRU 响应缓存
pub struct ResponseCache {
    inner: RwLock<CacheInner>,
    ttl: Duration,
    max_entries: usize,
    /// 单个响应 JSON 的最大字节数；超过此值直接不入缓存，避免被大响应撑爆内存
    max_response_bytes: usize,
}

impl ResponseCache {
    pub fn new(ttl_secs: u64, max_entries: usize, max_response_bytes: usize) -> Self {
        Self {
            inner: RwLock::new(CacheInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                hits: 0,
                misses: 0,
                stores: 0,
                evictions: 0,
                expirations: 0,
                oversize_skips: 0,
            }),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
            max_response_bytes,
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

    /// 写入缓存。响应过大时直接跳过（不入缓存）。
    pub fn put(&self, key: CacheKey, response: Value) {
        let mut inner = self.inner.write().unwrap();

        // 大小检查：超过上限直接不入缓存
        let approx_bytes = serde_json::to_string(&response)
            .map(|s| s.len())
            .unwrap_or(0);
        if approx_bytes > self.max_response_bytes {
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
        }
    }

    /// 清空缓存（一般用于运维/调试）
    #[allow(dead_code)]
    pub fn clear(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.entries.clear();
        inner.order.clear();
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

        cache.put(1, resp1.clone());
        cache.put(2, resp2.clone());
        // 命中 key=1 会把它挪到队尾（最新），队首是 key=2
        assert_eq!(cache.get(1), Some(resp1.clone()));
        // 写入 key=3 时淘汰最久未访问的 = key=2
        cache.put(3, resp3.clone());

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
        cache.put(1, json!({"k":1}));
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(cache.get(1), None);
        assert_eq!(cache.stats().expirations, 1);
    }

    #[test]
    fn test_oversize_skip() {
        let cache = ResponseCache::new(60, 10, 10); // 最多 10 字节
        cache.put(1, json!({"big":"this is definitely way larger than ten bytes"}));
        assert_eq!(cache.get(1), None);
        assert_eq!(cache.stats().oversize_skips, 1);
    }
}
