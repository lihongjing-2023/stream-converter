# Stream Converter 快速启动教程

> 本文档记录了如何在本机找到 CodeBuddy/腾讯云 AI IDE 的配置与认证信息，
> 并快速完成 stream-converter 的编译与启动。

---

## 一、配置信息在哪里

CodeBuddy 插件（腾讯云 AI IDE）的配置存放在本机用户目录下：

```
~/.codebuddy/local_storage/
```

其中有 3 个 `*.info` 文件：

| 文件 | 大小 | 格式 | 内容 |
|------|------|------|------|
| `entry_426965c41d8cfbf5b8a3bf013b1a0384.info` | ~169KB | **gzip+base64** | 产品核心配置（endpoint、token、认证头映射） |
| `entry_933d5543e80177622c17a73869c0fad7.info` | 65B | 纯文本 | 上游 endpoint（json 字符串） |
| `entry_d43e96994f944cfb77961c2ea7d04605.info` | ~9KB | 明文 JSON | 用户 ID、模型列表、agents 配置 |

> 注：文件名前的哈希值是动态生成的，后续机器上可能不同，请用 `ls ~/.codebuddy/local_storage/*.info` 按大小/内容识别。

---

## 二、如何读取配置

### 1. 解压 gzip+base64 的核心配置

```bash
# 文件内容是 '"H4sI...' 格式（首尾带引号的 base64）
cat ~/.codebuddy/local_storage/entry_426965c41d8cfbf5b8a3bf013b1a0384.info \
  | sed 's/^"//; s/"$//' \
  | base64 -d | gzip -dc
```

### 2. 直接查看明文配置

```bash
cat ~/.codebuddy/local_storage/entry_d43e96994f944cfb77961c2ea7d04605.info
```

### 3. 提取关键字段（一行命令）

```bash
# 提取 endpoint / token / 认证头配置
cat ~/.codebuddy/local_storage/entry_426965c41d8cfbf5b8a3bf013b1a0384.info \
  | sed 's/^"//; s/"$//' | base64 -d | gzip -dc \
  | grep -o -E '"(endpoint|productName|token|usernameHeader|tokenHeader|tokenType)"[^,}]*'
```

---

## 三、关键配置项说明

核心配置解压后是 CodeBuddy 产品配置 JSON，关键字段：

```json
{
  "productName": "CodeBuddy",
  "endpoint": "https://api.cnb.cool/peerless-general/stream-converter/-/ai-ide",
  "authentication": {
    "type": "custom-token",
    "attributes": {
      "usernameHeader": "X-User-Id",
      "tokenHeader": "Authorization",
      "tokenType": "bearerToken",
      "token": "<Bearer Token 明文>"
    }
  }
}
```

| 字段 | 含义 | 用于 |
|------|------|------|
| `endpoint` | 上游 base URL | 代理的 `UPSTREAM_URL` |
| `token` | 认证 token | 请求头 `Authorization: Bearer <token>` |
| `usernameHeader` | 用户 ID 头名 | 请求头 `X-User-Id` |
| `tokenHeader` / `tokenType` | 认证头格式 | `Authorization: Bearer ...` |

用户 ID 在明文文件 `entry_d43e...` 中：
```json
[{"userId":"2029125027124817920@cnb", ...}]
```

---

## 四、认证请求头（拼接规则）

实际请求上游时需要携带的头（与 VSCode 插件一致）：

```
Authorization: Bearer <token>
X-User-Id: <userId>
X-Product: SaaS
X-IDE-Name: VSCode
X-Requested-With: XMLHttpRequest
Content-Type: application/json
```

---

## 五、编译启动

### 1. 编译

```bash
# 加载 Rust 环境（若 cargo 不在 PATH）
source "$HOME/.cargo/env"

cd /workspace/stream-converter
cargo build --release
```

> 若缺少依赖：`apt-get install -y libssl-dev pkg-config build-essential`

### 2. 启动代理

```bash
cd /workspace/stream-converter

UPSTREAM_URL="https://api.cnb.cool/peerless-general/stream-converter/-/ai-ide/v2/chat/completions" \
./target/release/stream-converter &
```

> 注意：`UPSTREAM_URL` 必须带完整路径 `/v2/chat/completions`（不带 `/v1`！）。
> 因 `build_upstream_url` 逻辑：URL 含自定义路径时直接原样使用，不再拼接。

### 3. 验证

```bash
# 健康检查
curl -s http://127.0.0.1:18318/health
# => {"status":"ok"}

# 测试流式对话
curl -s -X POST "http://127.0.0.1:18318/v1/chat/completions" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <token>" \
  -H "X-User-Id: <userId>" \
  -H "X-Product: SaaS" \
  -H "X-IDE-Name: VSCode" \
  -H "X-Requested-With: XMLHttpRequest" \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"你好"}],"stream":true}'
```

---

## 六、常见问题

| 问题 | 原因 | 解决 |
|------|------|------|
| `Resource not found` | `UPSTREAM_URL` 少了 `/v2/chat/completions` | 补全路径 |
| `credentials have expired` | token 过期 | 重新读取 info 文件里的最新 token |
| `SlugId not match` | endpoint 仓库写错（如用了 `PMBOK-doc`） | 用 info 里的 `endpoint` 字段 |
| 上游 403 | `X-User-Id` 与 token 不匹配 | 从 `entry_d43e...` 取正确的 userId |

---

## 七、快速一句话（给 AI 的指令模板）

> 1. 读取 `~/.codebuddy/local_storage/` 下 gzip+base64 的 info 文件（`sed 's/^"//; s/"$//' | base64 -d | gzip -dc`），提取 `endpoint` 和 `token` 字段；
> 2. 从明文 info 文件提取 `userId`；
> 3. 用 `endpoint + "/v2/chat/completions"` 作为 `UPSTREAM_URL` 编译启动；
> 4. 用提取的 token / userId 组装请求头测试。
