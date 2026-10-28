# Stream Converter (Rust)

高性能 OpenAI 兼容流/非流格式转换代理，使用 Rust 重写。

github仓库地址：https://github.com/lihongjing-2023/stream-converter.git

## 性能对比

| 指标 | Python (FastAPI+uvicorn) | Rust (axum+tokio) |
|------|--------------------------|-------------------|
| 内存占用 | ~100MB+ | ~10MB |
| 单核吞吐 | ~1K req/s | ~30K+ req/s |
| 延迟 P99 | ~50ms | ~5ms |
| 二进制大小 | 需 Python 运行时 | ~8MB (静态链接) |

## 功能

- **流式请求**：SSE 透传，并规范化上游非标准字段（空字符串 delta、旧式 `function_call` 等）
- **非流请求**：内部强制走流式收集，组装完整 JSON 返回
- **透传认证**：不修改 Authorization 头
- **错误处理**：上游连接失败/超时/非 200 响应均有 SSE 错误事件
- **Tool Call Name 修复**：自动删除流式 tool_call 后续 chunk 中的空 name 字段，避免客户端（如 Codex CLI）拼接时覆盖正确的工具名
- **Tool Call 非流式透传**：非流式请求按 index 聚合流式 tool_calls 分片，完整返回 `tool_calls`；无文本且有工具调用时 `content` 输出 `null`，与 OpenAI 非流式行为一致
- **结束原因修复**：`finish_reason` 为空字符串时规范化为 `null`；收到 `[DONE]` 但整条流未出现合法结束原因时补发一个 `stop` 收尾 chunk（内容大概率完整，仅结束标记序列化异常）；**未收到 `[DONE]` 就断流（EOF/截断）时不再伪造 `stop`，改发 SSE `stream_error` 错误事件**（非流式返回 502），让下游按流错误自动重试，而非把截断误报为正常结束
- **推理内容透传**：非流式响应同样返回 `reasoning_content`，与流式保持一致；并透传上游真实的 `finish_reason`（如 `length` 截断）
- **usage 精简**：仅移除 `credit` 计费字段，其余 usage 字段原样透传

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `UPSTREAM_URL` | `http://127.0.0.1:8317` | 上游服务地址 |
| `TIMEOUT` | `600` | 请求超时(秒) |
| `DEBUG` | `false` | 调试模式开关（开启后日志级别降为 debug） |
| `DEBUG_FULL_BODY` | `true` | 调试模式下输出**完整**请求体/响应体（仅 `DEBUG=true` 时生效，设 `false` 只保留元信息日志） |
| `DEBUG_BODY_LIMIT` | `0` | 单条完整正文日志的最大字符数，`0` = 不限制（超限时按字符边界安全截断） |
| `PORT` | `18318` | 监听端口 |
| `FIX_TOOL_CALL_NAME` | `true` | 修复流式 tool_call name 覆盖 bug（设为 `false` 关闭） |

### 调试模式输出内容

`DEBUG=true` 时会输出完整输入与输出，便于深度调试：

| 日志标签 | 内容 |
|----------|------|
| `[xxx] Request headers` | 全部请求头（`authorization` 已脱敏为 前6位`****`后4位） |
| `[xxx] Request body` | 完整请求 JSON（含全部 messages 原文） |
| `[STREAM RAW #n]` | 上游返回的**原始** SSE 协议行 |
| `[STREAM OUT #n]` | 修复/规范化后**实际发给下游**的 SSE 协议行 |
| `[STREAM FULL RESPONSE BODY]` | **完整响应体**：下游实际收到的 SSE body 全文（所有 chunk 已拼好，无需手工拼） |
| `[STREAM FULL COMPLETION]` | 把流式**还原成等价非流式 JSON**（content / reasoning_content / tool_calls / usage / finish_reason），一次性看全 |
| `[STREAM FULL CONTENT]` | 整条流聚合出的模型正文（纯文本） |
| `[COLLECT RAW #n]` | 非流请求内部上游的原始 SSE 协议行 |
| `[COLLECT FULL CONTENT]` | 非流请求聚合出的模型正文 |
| `[xxx] Response body` | 完整响应 JSON（含 reasoning_content / tool_calls / usage） |
| `[xxx] Response body (cache hit)` | 缓存命中的完整响应 JSON |
| `[xxx] CACHE STORE \| request/response` | 写入缓存的完整请求/响应内容 |

> 流式请求的完整响应体看 `[STREAM FULL RESPONSE BODY]`（协议原文）与 `[STREAM FULL COMPLETION]`（还原后的 JSON）；
> 非流请求看 `[xxx] Response body`。两者都是一次性输出，不用再逐条 chunk 手工拼接。

> 注意：`DEBUG=true` 会把完整 prompt 与模型回复写入日志，可能包含敏感信息，排查完请及时关闭。
> 长上下文场景建议配合 `DEBUG_BODY_LIMIT`（如 `200000`）限制单条日志体积。

## 本地编译运行

```bash
# 编译 (release 模式)
cd stream-converter-rs
cargo build --release

# 运行
UPSTREAM_URL=http://127.0.0.1:8317 ./target/release/stream-converter
```

## Docker 运行

```bash
docker build -t stream-converter-rs .
docker run -d \
  -p 18318:18318 \
  -e UPSTREAM_URL=http://your-upstream:8317 \
  stream-converter-rs
```

## 自定义开发环境（云原生开发）

仓库根目录的 `.ide/Dockerfile` 定义了 CNB 云原生开发环境（CNB 会优先使用该文件构建开发镜像）：

- 以 `cnbcool/default-dev-env:latest`（内置 code-server/VSCode 界面）为基础，
  补齐 Rust 工具链（rustup 稳定版）、`build-essential`（gcc/cc 链接器）、`libssl-dev`、`pkg-config` 等编译依赖。

**下次启动云原生开发后即可直接编译运行，无需再手动安装环境：**

```bash
# 进入开发环境后
cargo build --release
UPSTREAM_URL="https://api.cnb.cool/peerless-general/stream-converter/-/ai-ide/v2/chat/completions" \
PORT=18318 DEBUG=true ./target/release/stream-converter &
```

## API

- `POST /v1/chat/completions` — OpenAI 兼容接口
- `GET /health` — 健康检查
