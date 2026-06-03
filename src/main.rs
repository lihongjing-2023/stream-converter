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

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

struct Config {
    upstream_url: String,
    timeout_secs: u64,
    debug: bool,
    port: u16,
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
                .unwrap_or(true),
            port: std::env::var("PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(18318),
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
    usage: Usage,
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

#[derive(Serialize, Deserialize)]
struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
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

// ---------------------------------------------------------------------------
// 流式转发（透传 SSE）
// ---------------------------------------------------------------------------

async fn forward_stream(
    client: &Client,
    url: &str,
    request_data: &Value,
    headers: HeaderMap,
    _debug: bool,
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
        let msg = format!(
            "data: {{\"error\": {{\"message\": \"Upstream {}: {}\", \"type\": \"upstream_error\"}}}}\n\ndata: [DONE]\n\n",
            status,
            body.chars().take(500).collect::<String>()
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(msg))
            .unwrap();
    }

    info!("[STREAM] Upstream responded: 200");

    // 创建一个 channel 来产生 SSE 流
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(256);

    // 后台任务：读取上游 SSE 并转发
    tokio::spawn(async move {
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

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

                        if line.starts_with("data: ") {
                            let json_str = line[6..].trim();
                            if !json_str.is_empty() && json_str != "[DONE]" {
                                if let Ok(data) = serde_json::from_str::<Value>(json_str) {
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
                                                info!(
                                                    "[STREAM] Finished: reason={:?}, chunks={}, total_len={}, time={:.2}s",
                                                    finish, chunk_count, full_response.len(), elapsed
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if tx.send(Ok(Bytes::from(format!("{}\n\n", line)))).await.is_err() {
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
                info!("[COLLECT] Received [DONE]");
                let elapsed = start.elapsed().as_secs_f64();
                info!(
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
                                info!(
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
                        info!("[COLLECT] Usage from upstream: {}", usage);
                    }
                }
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    info!(
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

    let body_bytes = match axum::body::to_bytes(request.into_body(), 10 * 1024 * 1024).await {
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

    if stream {
        // 流式透传
        data["stream"] = Value::Bool(true);
        info!("[{}] Mode: STREAM (passthrough)", request_id);
        forward_stream(&state.client, &target_url, &data, forward_headers, state.config.debug).await
    } else {
        // 非流 → 收集流式，组装非流 JSON
        info!("[{}] Mode: NON-STREAM (collect then respond)", request_id);
        if state.config.debug {
            debug!("[{}] Request body: {}", request_id, data);
        }

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

                let usage_obj = if usage.is_null() {
                    serde_json::json!({
                        "prompt_tokens": 0,
                        "completion_tokens": 0,
                        "total_tokens": 0
                    })
                } else {
                    usage
                };

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
                    usage: serde_json::from_value(usage_obj).unwrap_or(Usage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                    }),
                };

                Json(resp).into_response()
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

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    client: Client,
    config: Arc<Config>,
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
    println!("  Port:     {}", config.port);
    println!("  Upstream: {}", build_upstream_url(&config.upstream_url));
    println!("  Timeout:  {}s", config.timeout_secs);
    println!("{}\n", "=".repeat(55));

    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(config.timeout_secs))
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("Failed to create HTTP client");

    let state = AppState {
        client,
        config: Arc::new(config),
    };
    let port = state.config.port;

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/health", get(health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .expect("Failed to bind port");

    info!("Listening on 0.0.0.0:{}", port);
    axum::serve(listener, app).await.expect("Server error");
}
