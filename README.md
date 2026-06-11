# Stream Converter (Rust)

高性能 OpenAI 兼容流/非流格式转换代理，使用 Rust 重写。

## 性能对比

| 指标 | Python (FastAPI+uvicorn) | Rust (axum+tokio) |
|------|--------------------------|-------------------|
| 内存占用 | ~100MB+ | ~10MB |
| 单核吞吐 | ~1K req/s | ~30K+ req/s |
| 延迟 P99 | ~50ms | ~5ms |
| 二进制大小 | 需 Python 运行时 | ~8MB (静态链接) |

## 功能

- **流式请求**：原样透传 SSE 给下游
- **非流请求**：内部强制走流式收集，组装完整 JSON 返回
- **透传认证**：不修改 Authorization 头
- **错误处理**：上游连接失败/超时/非 200 响应均有 SSE 错误事件
- **Tool Call Name 修复**：自动删除流式 tool_call 后续 chunk 中的空 name 字段，避免客户端（如 Codex CLI）拼接时覆盖正确的工具名

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `UPSTREAM_URL` | `http://127.0.0.1:8317` | 上游服务地址 |
| `TIMEOUT` | `600` | 请求超时(秒) |
| `DEBUG` | `true` | 调试日志 |
| `PORT` | `18318` | 监听端口 |
| `FIX_TOOL_CALL_NAME` | `true` | 修复流式 tool_call name 覆盖 bug（设为 `false` 关闭） |

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

## API

- `POST /v1/chat/completions` — OpenAI 兼容接口
- `GET /health` — 健康检查
