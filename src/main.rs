use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use chrono::Utc;
use futures::stream::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info};
use uuid::Uuid;

mod cache;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 请求体最大大小（支持 100 万 token 级别上下文）
const MAX_BODY_SIZE: usize = 64 * 1024 * 1024; // 64 MB

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

struct Config {
    upstream_url: String,
    timeout_secs: u64,
    debug: bool,
    port: u16,
    fix_tool_call_name: bool,
    cache_hit_discount: f64,
    // ---- 响应缓存 ----
    /// 是否启用响应缓存
    cache_enabled: bool,
    /// 缓存条目 TTL（秒）
    cache_ttl_secs: u64,
    /// 最大缓存条目数（LRU 淘汰）
    cache_max_entries: usize,
    /// 单个请求体最大字节数（请求体超出则对应响应不入缓存）
    cache_max_request_bytes: usize,
    /// 预热阈值：窗口内出现该次数才允许命中缓存（0 = 关闭预热，立即生效）
    cache_min_hit_count: u32,
    /// 预热窗口（秒）
    cache_warmup_window_secs: u64,
}

impl Config {
    fn from_env() -> Self {
        Self {
            upstream_url: std::env::var("UPSTREAM_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8317".into()),
            timeout_secs: std::env::var("TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
            debug: std::env::var("DEBUG")
                .ok()
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false),
            port: std::env::var("PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(18318),
            fix_tool_call_name: std::env::var("FIX_TOOL_CALL_NAME")
                .ok()
                .map(|v| v.to_lowercase() != "false")
                .unwrap_or(true),
            cache_hit_discount: std::env::var("CACHE_HIT_DISCOUNT")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(1.0),
            // 默认关闭，避免对存量业务产生影响；启用后按需调整阈值
            cache_enabled: std::env::var("CACHE_ENABLED")
                .ok()
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false),
            cache_ttl_secs: std::env::var("CACHE_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            cache_max_entries: std::env::var("CACHE_MAX_ENTRIES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),
            cache_max_request_bytes: std::env::var("CACHE_MAX_REQUEST_BYTES")
                .ok()
                // 向后兼容旧变量名 CACHE_MAX_RESPONSE_BYTES
                .or_else(|| std::env::var("CACHE_MAX_RESPONSE_BYTES").ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(100 * 1024),
            cache_min_hit_count: std::env::var("CACHE_MIN_HIT_COUNT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            cache_warmup_window_secs: std::env::var("CACHE_WARMUP_WINDOW_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(180),
        }
    }
}

// ---------------------------------------------------------------------------
// 请求/响应类型
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[allow(dead_code)]
struct ChatRequest {
    model: Option<String>,
    stream: Option<bool>,
    messages: Option<Vec<Value>>,
    #[serde(flatten)]
    extra: Value,
}

#[derive(Serialize)]
struct ChatCompletionResponse {
    id: String,
    object: String,
    created: i64,
    model: String,
    choices: Vec<Choice>,
    usage: Value,
}

#[derive(Serialize)]
struct Choice {
    index: u32,
    message: Message,
    finish_reason: String,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
}

// ---------------------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------------------

fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return "(empty)".into();
    }
    if key.len() <= 12 {
        format!("{}***", &key[..4.min(key.len())])
    } else {
        format!("{}****{}", &key[..6], &key[key.len() - 4..])
    }
}

/// 根据 upstream URL 构建目标请求 URL。
///
/// - 如果 upstream 已有自定义路径（如 `http://host/custom-route`），直接使用原 URL。
/// - 如果 upstream 只有 host:port 或域名（无有效路径），自动拼接 `/v1/chat/completions`。
fn build_upstream_url(base: &str) -> String {
    if let Ok(parsed) = url::Url::parse(base) {
        // path() 至少返回 "/"，若长度 > 1 或含 query，说明有自定义路由
        if parsed.path().len() > 1 || parsed.query().is_some() {
            return base.to_string();
        }
    }
    format!("{}/v1/chat/completions", base.trim_end_matches('/'))
}

fn error_json(msg: &str, error_type: &str, status: StatusCode) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: ErrorDetail {
                message: msg.into(),
                error_type: error_type.into(),
            },
        }),
    )
        .into_response()
}

fn extract_openai_content(data: &Value) -> Option<&str> {
    data.get("choices")?
        .get(0)?
        .get("delta")?
        .get("content")?
        .as_str()
}

/// 修复流式 tool_call 的 name 被后续空字符串覆盖的 bug。
///
/// OpenAI 流式协议中，tool_call 的 name 只在第一个 chunk 出现，
/// 后续 chunk 的 function.name 为 "" 。某些客户端（如 Codex CLI）
/// 在拼接时会用空字符串覆盖正确的 name。
///
/// 修复策略：记录每个 index 首次出现的非空 name，后续 chunk 中
/// 如果 name 为空字符串则删除该字段，避免客户端覆盖。
///
/// 返回 true 表示 data 被修改过。
fn fix_tool_call_name_overwrite(
    data: &mut Value,
    seen_names: &mut std::collections::HashMap<u32, String>,
) -> bool {
    let mut modified = false;

    if let Some(choices) = data.get_mut("choices") {
        if let Some(choices_arr) = choices.as_array_mut() {
            for choice in choices_arr.iter_mut() {
                if let Some(delta) = choice.get_mut("delta") {
                    if let Some(tool_calls) = delta.get_mut("tool_calls") {
                        if let Some(tc_arr) = tool_calls.as_array_mut() {
                            for tc in tc_arr.iter_mut() {
                                let idx = tc
                                    .get("index")
                                    .and_then(|i| i.as_u64())
                                    .unwrap_or(0) as u32;

                                if let Some(func) = tc.get_mut("function") {
                                    if let Some(obj) = func.as_object_mut() {
                                        if let Some(name_val) = obj.get("name") {
                                            if let Some(name_str) = name_val.as_str() {
                                                if !name_str.is_empty() {
                                                    // 首次出现非空 name，记录
                                                    seen_names
                                                        .insert(idx, name_str.to_string());
                                                } else if seen_names.contains_key(&idx) {
                                                    // 后续 chunk 的空 name，删除
                                                    obj.remove("name");
                                                    modified = true;
                                                    debug!(
                                                        "[FIX] Removed empty name for tool_call index {}",
                                                        idx
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    modified
}

/// 对缓存命中数统一打折扣（例如 ×0.5 即减半）。
///
/// 折扣比例通过环境变量 `CACHE_HIT_DISCOUNT` 配置，默认 1.0（不打折）。
///
/// 被折扣的字段：
/// - `prompt_cache_hit_tokens`（顶层）
/// - `prompt_tokens_details.cached_tokens`（同步更新）
fn apply_cache_hit_discount(usage: &mut Value, discount: f64) {
    if (discount - 1.0).abs() < f64::EPSILON {
        return;
    }

    // 顶层 prompt_cache_hit_tokens
    if let Some(val) = usage.get_mut("prompt_cache_hit_tokens") {
        if let Some(n) = val.as_f64() {
            let reduced = (n * discount).round() as u64;
            debug!("[CACHE_DISCOUNT] prompt_cache_hit_tokens: {} → {} (×{})", n as u64, reduced, discount);
            *val = Value::Number(serde_json::Number::from(reduced));
        }
    }

    // 同步 prompt_tokens_details.cached_tokens
    if let Some(details) = usage.get_mut("prompt_tokens_details") {
        if let Some(val) = details.get_mut("cached_tokens") {
            if let Some(n) = val.as_f64() {
                let reduced = (n * discount).round() as u64;
                debug!("[CACHE_DISCOUNT] prompt_tokens_details.cached_tokens: {} → {} (×{})", n as u64, reduced, discount);
                *val = Value::Number(serde_json::Number::from(reduced));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 流式转发（透传 SSE）
// ---------------------------------------------------------------------------

async fn forward_stream(
    client: &Client,
    url: &str,
    request_data: &Value,
    headers: HeaderMap,
    _debug: bool,
    fix_tool_call_name: bool,
) -> Response {
    let start = Instant::now();
    let mut chunk_count: u64 = 0;
    let mut full_response = String::new();

    let resp = match client
        .post(url)
        .json(request_data)
        .headers(headers)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if e.is_connect() {
                error!("[STREAM] Connection failed: {}", e);
                let msg = format!("data: {{\"error\": {{\"message\": \"Connection failed: {}\", \"type\": \"connect_error\"}}}}\n\ndata: [DONE]\n\n", e);
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(msg))
                    .unwrap();
            }
            if e.is_timeout() {
                error!("[STREAM] Read timeout from upstream");
                let msg = "data: {\"error\": {\"message\": \"Read timeout from upstream\", \"type\": \"timeout\"}}\n\ndata: [DONE]\n\n";
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from(msg))
                    .unwrap();
            }
            error!("[STREAM] Request error: {}", e);
            let msg = format!("data: {{\"error\": {{\"message\": \"{}: {}\", \"type\": \"stream_error\"}}}}\n\ndata: [DONE]\n\n", 
                std::any::type_name::<reqwest::Error>(), e);
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(msg))
                .unwrap();
        }
    };

    if resp.status() != StatusCode::OK {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        error!("[STREAM] Upstream error {}: {}", status, &body[..body.len().min(500)]);
        // 透传上游的 HTTP 状态码和原始错误 body，而不是包装成 SSE 200
        return Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
    }

    info!("[STREAM] Upstream responded: 200");

    // 创建一个 channel 来产生 SSE 流
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(256);

    // 后台任务：读取上游 SSE 并转发
    tokio::spawn(async move {
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut tool_call_names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));

                    // 按行处理
                    while let Some(newline_pos) = buffer.find('\n') {
                        let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                        buffer = buffer[newline_pos + 1..].to_string();

                        if line.is_empty() {
                            continue;
                        }

                        // 尝试解析并修复 SSE data 行
                        let output_line = if line.starts_with("data: ") {
                            let json_str = line[6..].trim();
                            if !json_str.is_empty() && json_str != "[DONE]" {
                                match serde_json::from_str::<Value>(json_str) {
                                    Ok(mut data) => {
                                        if let Some(content) = extract_openai_content(&data) {
                                            chunk_count += 1;
                                            full_response.push_str(content);
                                        }
                                        if let Some(choices) = data.get("choices") {
                                            if let Some(finish) =
                                                choices.get(0).and_then(|c| c.get("finish_reason"))
                                            {
                                                if !finish.is_null() {
                                                    let elapsed = start.elapsed().as_secs_f64();
                                                    debug!(
                                                        "[STREAM] Finished: reason={:?}, chunks={}, total_len={}, time={:.2}s",
                                                        finish, chunk_count, full_response.len(), elapsed
                                                    );
                                                }
                                            }
                                        }

                                        // 修复 tool_call name 覆盖 bug
                                        if fix_tool_call_name
                                            && fix_tool_call_name_overwrite(&mut data, &mut tool_call_names)
                                        {
                                            match serde_json::to_string(&data) {
                                                Ok(fixed_json) => format!("data: {}", fixed_json),
                                                Err(_) => line.clone(),
                                            }
                                        } else {
                                            line.clone()
                                        }
                                    }
                                    Err(_) => line.clone(),
                                }
                            } else {
                                line.clone()
                            }
                        } else {
                            line.clone()
                        };

                        if tx.send(Ok(Bytes::from(format!("{}\n\n", output_line)))).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("[STREAM] Stream read error: {}", e);
                    let _ = tx
                        .send(Ok(Bytes::from(format!(
                            "data: {{\"error\": {{\"message\": \"Stream error: {}\", \"type\": \"stream_error\"}}}}\n\n",
                            e
                        ))))
                        .await;
                    let _ = tx.send(Ok(Bytes::from("data: [DONE]\n\n"))).await;
                    break;
                }
            }
        }

        // 处理 buffer 中可能剩余的内容
        if !buffer.trim().is_empty() {
            let _ = tx.send(Ok(Bytes::from(format!("{}\n\n", buffer.trim())))).await;
        }
    });

    let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(body_stream);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .unwrap()
}

// ---------------------------------------------------------------------------
// 非流 → 流转换（收集流式响应，组装非流 JSON）
// ---------------------------------------------------------------------------

async fn collect_stream(
    client: &Client,
    url: &str,
    request_data: &mut Value,
    headers: HeaderMap,
) -> Result<(String, String, Value), String> {
    let start = Instant::now();
    let mut full_content = String::new();
    let mut model_name = String::new();
    let mut usage = Value::Null;
    let mut chunk_count: u64 = 0;

    // 强制开启流式
    request_data["stream"] = Value::Bool(true);

    let resp = client
        .post(url)
        .json(&*request_data)
        .headers(headers)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| format!("Connection failed: {}", e))?;

    info!("[COLLECT] Upstream responded: {}", resp.status());

    if resp.status() != StatusCode::OK {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Upstream error: {} - {}", status, body));
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk_result) = stream.next().await {
        let bytes = chunk_result.map_err(|e| format!("Stream read error: {}", e))?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
            buffer = buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                continue;
            }

            let data_str = if line.starts_with("data: ") {
                &line[6..]
            } else {
                &line
            };

            let data_str = data_str.trim();

            if data_str == "[DONE]" {
                debug!("[COLLECT] Received [DONE]");
                let elapsed = start.elapsed().as_secs_f64();
                debug!(
                    "[COLLECT] Done: {} chunks, {} chars, {:.2}s",
                    chunk_count,
                    full_content.len(),
                    elapsed
                );
                return Ok((full_content, model_name, usage));
            }

            if let Ok(data) = serde_json::from_str::<Value>(data_str) {
                if let Some(m) = data.get("model").and_then(|v| v.as_str()) {
                    model_name = m.to_string();
                }

                if let Some(choices) = data.get("choices") {
                    if let Some(choice) = choices.get(0) {
                        // delta content
                        if let Some(content) = choice
                            .get("delta")
                            .and_then(|d| d.get("content"))
                            .and_then(|c| c.as_str())
                        {
                            chunk_count += 1;
                            full_content.push_str(content);
                        }

                        // message content（某些非标准实现）
                        if let Some(content) = choice
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_str())
                        {
                            if !content.is_empty() {
                                chunk_count += 1;
                                full_content.push_str(content);
                            }
                        }

                        // finish_reason
                        if let Some(finish) = choice.get("finish_reason") {
                            if !finish.is_null() {
                                let elapsed = start.elapsed().as_secs_f64();
                                debug!(
                                    "[COLLECT] Finished: reason={:?}, chunks={}, total_len={}, time={:.2}s",
                                    finish, chunk_count, full_content.len(), elapsed
                                );
                            }
                        }
                    }
                }

                if let Some(u) = data.get("usage") {
                    if !u.is_null() {
                        usage = u.clone();
                        debug!("[COLLECT] Usage from upstream: {}", usage);
                    }
                }
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    debug!(
        "[COLLECT] Done (stream ended): {} chunks, {} chars, {:.2}s",
        chunk_count,
        full_content.len(),
        elapsed
    );
    Ok((full_content, model_name, usage))
}

// ---------------------------------------------------------------------------
// API 端点
// ---------------------------------------------------------------------------

async fn chat_completions(
    axum::extract::State(state): axum::extract::State<AppState>,
    req_headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = Uuid::new_v4().to_string()[..8].to_string();
    let start = Instant::now();

    let body_bytes = match axum::body::to_bytes(request.into_body(), MAX_BODY_SIZE).await {
        Ok(b) => b,
        Err(e) => {
            error!("[{}] Failed to read request body: {}", request_id, e);
            return error_json("Failed to read request body", "invalid_request", StatusCode::BAD_REQUEST);
        }
    };

    let mut data: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            error!("[{}] Invalid JSON: {}", request_id, e);
            return error_json(&format!("Invalid JSON: {}", e), "invalid_request", StatusCode::BAD_REQUEST);
        }
    };

    let model = data.get("model").and_then(|v| v.as_str()).unwrap_or("(unknown)").to_string();
    let stream = data.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let msg_count = data.get("messages").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);

    // 脱敏记录认证信息
    let auth_raw = req_headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let auth_masked = if !auth_raw.is_empty() {
        let key = auth_raw.trim_start_matches("Bearer ");
        mask_key(key)
    } else {
        "(none)".into()
    };

    info!(
        "[{}] IN  | model={} | stream={} | messages={} | auth={}",
        request_id, model, stream, msg_count, auth_masked
    );

    if state.config.debug {
        let mut headers_log = std::collections::HashMap::new();
        for (k, v) in req_headers.iter() {
            if k == "authorization" {
                headers_log.insert(k.as_str().to_string(), format!("Bearer {}", auth_masked));
            } else {
                headers_log.insert(k.as_str().to_string(), v.to_str().unwrap_or("?").to_string());
            }
        }
        debug!("[{}] Request headers: {:?}", request_id, headers_log);
        debug!("[{}] Request body: {}", request_id, data);
    }

    // 构建转发请求头
    let mut forward_headers = HeaderMap::new();
    forward_headers.insert("content-type", HeaderValue::from_static("application/json"));
    if !auth_raw.is_empty() {
        if let Ok(val) = HeaderValue::from_str(auth_raw) {
            forward_headers.insert("authorization", val);
        }
    }

    let target_url = build_upstream_url(&state.config.upstream_url);
    info!("[{}] Target upstream: {}", request_id, target_url);

    // 计算缓存 key（仅在启用缓存时计算）
    let cache_key = if state.config.cache_enabled && !stream {
        cache::compute_cache_key(&data)
    } else {
        None
    };

    // 缓存命中：直接返回，跳过上游调用（需先完成预热，即同一请求+同一响应在窗口内累计达标）
    if let Some(key) = cache_key {
        if state.cache.is_warmed_up(key) {
            if let Some(cached_resp) = state.cache.get(key) {
                // 模拟处理耗时：随机睡眠 0.3~0.8s，避免缓存命中响应过快而暴露缓存行为
                let delay_ms: u64 = rand::Rng::gen_range(&mut rand::thread_rng(), 300..=800);
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let elapsed = start.elapsed().as_secs_f64();
                info!(
                    "[{}] OUT | CACHE HIT  | key={:016x} | time={:.3}s (sim delay={}ms) | saved 1 upstream call",
                    request_id, key, elapsed, delay_ms
                );
                let mut resp = Json(cached_resp).into_response();
                // 标记响应来自缓存，便于客户端/网关识别
                resp.headers_mut()
                    .insert("x-cache", HeaderValue::from_static("HIT"));
                return resp;
            }
            debug!("[{}] cache miss (entry absent) | key={:016x}", request_id, key);
        } else {
            debug!("[{}] cache warming up | key={:016x}", request_id, key);
        }
    }

    if stream {
        // 流式透传
        data["stream"] = Value::Bool(true);
        info!("[{}] Mode: STREAM (passthrough)", request_id);
        forward_stream(&state.client, &target_url, &data, forward_headers, state.config.debug, state.config.fix_tool_call_name).await
    } else {
        // 非流 → 收集流式，组装非流 JSON
        info!("[{}] Mode: NON-STREAM (collect then respond)", request_id);

        match collect_stream(&state.client, &target_url, &mut data, forward_headers).await {
            Ok((full_content, model_name, usage)) => {
                let elapsed = start.elapsed().as_secs_f64();
                info!(
                    "[{}] OUT | model={} | content_len={} | time={:.2}s",
                    request_id,
                    if model_name.is_empty() { &model } else { &model_name },
                    full_content.len(),
                    elapsed
                );

                let mut usage_obj = if usage.is_null() {
                    serde_json::json!({
                        "prompt_tokens": 0,
                        "completion_tokens": 0,
                        "total_tokens": 0
                    })
                } else {
                    usage
                };

                // 统一折扣（非流式响应降低缓存命中数）
                apply_cache_hit_discount(&mut usage_obj, state.config.cache_hit_discount);

                let resp = ChatCompletionResponse {
                    id: format!("chatcmpl-{}", Uuid::new_v4()),
                    object: "chat.completion".into(),
                    created: Utc::now().timestamp(),
                    model: if model_name.is_empty() { model.clone() } else { model_name },
                    choices: vec![Choice {
                        index: 0,
                        message: Message {
                            role: "assistant".into(),
                            content: full_content,
                        },
                        finish_reason: "stop".into(),
                    }],
                    usage: usage_obj,
                };

                // 序列化为 Value 同时用于写缓存 + 响应（一次序列化 + 一次克隆）
                let resp_value = serde_json::to_value(&resp)
                    .expect("ChatCompletionResponse serialization should not fail");

                // 写入缓存（仅当 key 存在）；以请求体大小作为缓存占用成本判断依据
                if let Some(key) = cache_key {
                    // 预热累计：同一请求+同一响应在窗口内累计到阈值后返回 true，才写入缓存
                    if state.cache.record_occurrence(key, &resp_value) {
                        state.cache.put(key, resp_value.clone(), body_bytes.len());
                        info!(
                            "[{}] CACHE STORE | key={:016x} | req_bytes={}",
                            request_id, key, body_bytes.len()
                        );
                    }
                }

                Json(resp_value).into_response()
            }
            Err(e) => {
                let elapsed = start.elapsed().as_secs_f64();
                error!("[{}] Collect error after {:.1}s: {}", request_id, elapsed, e);
                error_json(&e, "upstream_error", StatusCode::BAD_GATEWAY)
            }
        }
    }
}

async fn health() -> Response {
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// 返回当前响应缓存统计指标（命中/未命中/淘汰/过期等）。
async fn cache_stats(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let stats = state.cache.stats();
    Json(serde_json::json!({
        "enabled": state.config.cache_enabled,
        "max_entries": stats.max_size,
        "ttl_secs": stats.ttl_secs,
        "size": stats.current_size,
        "hits": stats.hits,
        "misses": stats.misses,
        "hit_rate": stats.hit_rate(),
        "stores": stats.stores,
        "evictions": stats.evictions,
        "expirations": stats.expirations,
        "oversize_skips": stats.oversize_skips,
        "warming_keys": stats.warming_keys,
        "min_hit_count": stats.min_hit_count,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    client: Client,
    config: Arc<Config>,
    /// 进程内响应缓存（仅用于非流请求）
    cache: Arc<cache::ResponseCache>,
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let config = Config::from_env();

    // 初始化日志
    let log_level = if config.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .init();

    println!("\n{}", "=".repeat(55));
    println!("  Stream Converter (Rust) 正在启动...");
    println!("  Port:              {}", config.port);
    println!("  Upstream:          {}", build_upstream_url(&config.upstream_url));
    println!("  Timeout:           {}s", config.timeout_secs);
    println!("  Fix ToolCall Name: {}", config.fix_tool_call_name);
    if (config.cache_hit_discount - 1.0).abs() > f64::EPSILON {
        println!("  Cache Discount:    ×{}", config.cache_hit_discount);
    }
    if config.cache_enabled {
        println!(
            "  Response Cache:    ENABLED (TTL={}s, max_entries={}, max_req={}KB)",
            config.cache_ttl_secs,
            config.cache_max_entries,
            config.cache_max_request_bytes / 1024
        );
        if config.cache_min_hit_count > 0 {
            println!(
                "  Cache Warmup:      ON (≥{} 次 / {}s 窗口内才命中)",
                config.cache_min_hit_count,
                config.cache_warmup_window_secs
            );
        } else {
            println!("  Cache Warmup:      OFF (立即命中)");
        }
    } else {
        println!("  Response Cache:    DISABLED (set CACHE_ENABLED=true to enable)");
    }
    println!("{}\n", "=".repeat(55));

    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(config.timeout_secs))
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("Failed to create HTTP client");

    let cache_max_entries = config.cache_max_entries;
    let cache_ttl_secs = config.cache_ttl_secs;
    let cache_max_request_bytes = config.cache_max_request_bytes;
    let cache_min_hit_count = config.cache_min_hit_count;
    let cache_warmup_window_secs = config.cache_warmup_window_secs;
    let mut cache = cache::ResponseCache::new(
        cache_ttl_secs,
        cache_max_entries,
        cache_max_request_bytes,
    );
    if cache_min_hit_count > 0 {
        cache = cache.with_warmup(
            cache_min_hit_count,
            std::time::Duration::from_secs(cache_warmup_window_secs),
        );
    }
    let state = AppState {
        client,
        config: Arc::new(config),
        cache: Arc::new(cache),
    };
    let port = state.config.port;

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/health", get(health))
        .route("/v1/cache/stats", get(cache_stats))
        .with_state(state.clone());

    // 周期打印缓存统计（仅在启用缓存时有意义）
    if state.config.cache_enabled {
        let cache = state.cache.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            // 第一次 tick 立即触发，跳过这一次避免启动时立即打印
            interval.tick().await;
            loop {
                interval.tick().await;
                let s = cache.stats();
                info!(
                    "[CACHE STATS] size={}/{} | warming={} | hits={} misses={} hit_rate={:.1}% | stores={} evictions={} expirations={} oversize_skips={}",
                    s.current_size, s.max_size, s.warming_keys, s.hits, s.misses,
                    s.hit_rate() * 100.0, s.stores, s.evictions, s.expirations, s.oversize_skips
                );
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .expect("Failed to bind port");

    info!("Listening on 0.0.0.0:{}", port);
    axum::serve(listener, app).await.expect("Server error");
}
