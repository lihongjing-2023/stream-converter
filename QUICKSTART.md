# Stream Converter 快速启动教程

> 本文档记录 stream-converter 的编译、启动、认证与公网访问方法。
>
> **核心结论（重要）**：
> - 程序需要访问上游 AI IDE 接口，认证用的是 **CNB 环境变量里注入的 token**，请求头为
>   `Authorization: Bearer $CNB_TOKEN`，程序会把该头原样透传给上游；
> - 程序运行后，公网访问地址由 `CNB_VSCODE_PROXY_URI` 提供，把其中的 `{{port}}` 替换成程序监听端口即可。

---

## 一、认证信息与环境变量

认证与配置信息全部来自 CNB 云原生构建环境自动注入的**环境变量**，无需手动生成。

### 1. 直接查看

```bash
# token（API 访问令牌）—— 认证头 Authorization 的 token
echo "$CNB_TOKEN"

# 上游 endpoint（完整产品配置 JSON，含 endpoint + token）
echo "$ACC_PRODUCT_CONFIG_V2"

# 公网映射域名模板（把 {{port}} 替换成实际端口）
echo "$CNB_VSCODE_PROXY_URI"
```

### 2. 关键变量一览

| 环境变量 | 说明 | 当前环境中的值 |
|---------|------|---------------|
| `CNB_TOKEN` | CNB API 访问 token，**认证头用** | `d4mYeaO8b1pask08f1QY4DveeyM` |
| `ACC_PRODUCT_CONFIG_V2` | AI IDE 产品配置（含 `endpoint` + `token`） | `{"endpoint":"https://api.cnb.cool/peerless-general/stream-converter/-/ai-ide",...}` |
| `TWINE_PASSWORD` | 制品库发布密码（复用同一 token，非本程序所需） | `d4mYeaO8b1pask08f1QY4DveeyM` |
| `CNB_VSCODE_PROXY_URI` | **公网访问域名模板**，程序运行后可访问的地址 | `https://ta4d659o9t-{{port}}.cnb.run/` |
| `ACC_USER_ID` | 用户 ID（同 `userId`） | `2029125027124817920@cnb` |
| `CNB_REPO_SLUG` | 仓库定位 | `peerless-general/stream-converter` |

> token 会随环境变化，请始终通过 `echo "$CNB_TOKEN"` 动态获取，不要硬编码到配置文件。

---

## 二、程序如何认证（读代码得出的结论）

`src/main.rs` 中，服务收到请求后会把请求头里的 `Authorization` 头**原样透传**给上游：

```rust
// src/main.rs  chat_completions()
let auth_raw = req_headers.get("authorization")...;   // 读取请求头 Authorization
forward_headers.insert("authorization", val);          // 原样转发给上游
```

因此：
- 客户端调用本程序时，只需携带 `Authorization: Bearer <token>` 头；
- 该头会被完整转发到上游完成认证；
- token 用 `$CNB_TOKEN` 的值即可。

---

## 三、编译

```bash
# 加载 Rust 环境（若 cargo 不在 PATH）
source "$HOME/.cargo/env"

cd /workspace
cargo build --release
```

> 若缺少依赖：`apt-get install -y libssl-dev pkg-config build-essential`

---

## 四、启动

`UPSTREAM_URL` 指向上游 AI IDE 接口。从 `ACC_PRODUCT_CONFIG_V2` 的 `endpoint` 字段取基础地址，
再按下面的规则拼接：

- `build_upstream_url` 逻辑：URL 若带自定义路径则**原样使用**，否则自动拼接 `/v1/chat/completions`；
- 所以这里显式带上完整路径 `/v2/chat/completions`（不要用 `/v1`）。

启动方式**推荐使用项目自带的守护进程管理脚本** `manage-stream-amd64.sh`，
它用 `nohup` 启动，**关闭控制台后程序依然在后台运行**，并可随时查看状态/日志/停止。

> ⚠️ 管理脚本默认查找二进制名 `stream-converter-linux-amd64`，与编译产物 `target/release/stream-converter` 不同。
> 首次使用前先把二进制拷贝/改名为该名字（见下方示例）。

### 方式一（推荐）：守护进程脚本 `manage-stream-amd64.sh`

```bash
cd /workspace

# 0) 一次性准备：赋执行权限 + 让脚本找到编译好的二进制
chmod +x manage-stream-amd64.sh
cp -f target/release/stream-converter ./stream-converter-linux-amd64

# 1) 后台启动实例（端口 18318，关控制台不退出）
./manage-stream-amd64.sh start \
  --port 18318 \
  --upstream-url "https://api.cnb.cool/peerless-general/stream-converter/-/ai-ide/v2/chat/completions" \
  --debug

# 2) 查看状态 / 日志 / 停止
./manage-stream-amd64.sh status --port 18318   # 查看是否运行
./manage-stream-amd64.sh log --port 18318      # 查看日志（Ctrl+C 退出查看）
./manage-stream-amd64.sh stop  --port 18318    # 停止实例
./manage-stream-amd64.sh list                  # 列出所有实例
```

脚本支持多实例：不同 `--port` 对应不同 `--upstream-url`，互不影响。

### 方式二（临时前台）：直接运行

仅用于临时调试（关闭终端进程即退出）：

```bash
cd /workspace

UPSTREAM_URL="https://api.cnb.cool/peerless-general/stream-converter/-/ai-ide/v2/chat/completions" \
PORT=18318 \
DEBUG=true \
./target/release/stream-converter
```

> 注意：末尾**不要**加 `&`（那只是当前 shell 的后台 job，关闭控制台会因 SIGHUP 被杀），
> 要用 `nohup` 或管理脚本才能常驻后台。

启动成功后程序监听 `0.0.0.0:18318`（端口可用 `PORT` 环境变量覆盖）。

---

## 五、验证

### 1. 本地健康检查

```bash
curl -s http://127.0.0.1:18318/health
# => {"status":"ok"}
```

### 2. 本地测试流式对话（用环境变量里的 token）

```bash
curl -s -X POST "http://127.0.0.1:18318/v1/chat/completions" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $CNB_TOKEN" \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"你好"}],"stream":true}'
```

---

## 六、公网访问（程序运行后可访问的地址）

公网地址 = `$CNB_VSCODE_PROXY_URI` 中的 `{{port}}` 替换成程序实际监听端口。

```bash
echo "$CNB_VSCODE_PROXY_URI"
# => https://ta4d659o9t-{{port}}.cnb.run/
```

若程序监听 `18318` 端口，则公网地址为：

```
https://ta4d659o9t-18318.cnb.run/
```

公网调用示例：

```bash
curl -s -X POST "https://ta4d659o9t-18318.cnb.run/v1/chat/completions" \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $CNB_TOKEN" \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"你好"}],"stream":true}'
```

---

## 七、常见问题

| 问题 | 原因 | 解决 |
|------|------|------|
| `Resource not found` | `UPSTREAM_URL` 少了 `/v2/chat/completions` | 补全路径 |
| `credentials have expired` | token 过期 | 重新 `echo "$CNB_TOKEN"` 取最新 token |
| 上游 401/403 | `Authorization` 头缺失或 token 错误 | 用 `Bearer $CNB_TOKEN` 传认证头 |
| 公网无法访问 | 端口没替换，或程序没监听 | 把 `{{port}}` 替换为真实监听端口，确认程序已启动 |
| 关闭控制台程序就没了 | 用 `&` 只是 shell 后台 job，会话结束被杀 | 用 `manage-stream-amd64.sh`（nohup 常驻）或 `nohup ./target/release/stream-converter ...` |
| 脚本提示找不到二进制 | 脚本找 `stream-converter-linux-amd64` | `cp -f target/release/stream-converter ./stream-converter-linux-amd64` |

---

## 八、快速一句话（给 AI 的指令模板）

> 1. token：`echo "$CNB_TOKEN"`，调用时用 `Authorization: Bearer $CNB_TOKEN`；
> 2. 常驻启动：`chmod +x manage-stream-amd64.sh`、`cp -f target/release/stream-converter ./stream-converter-linux-amd64` 后
>    `./manage-stream-amd64.sh start --port 18318 --upstream-url "https://api.cnb.cool/peerless-general/stream-converter/-/ai-ide/v2/chat/completions" --debug`；
> 3. 公网访问：把 `$CNB_VSCODE_PROXY_URI`（`https://ta4d659o9t-{{port}}.cnb.run/`）中的 `{{port}}` 换成监听端口；
> 4. 调用：`curl ... -H "Authorization: Bearer $CNB_TOKEN" -d '{"stream":true,...}'`。
