#!/bin/bash

# Stream Converter (AMD64 Binary) 多实例管理脚本
# 支持同时运行多个实例，不同端口对应不同上游地址
#
# 用法:
#   ./manage-stream-amd64.sh start  --port 18318 --upstream-url http://127.0.0.1:8317
#   ./manage-stream-amd64.sh start  --port 18319 --upstream-url http://127.0.0.1:9000
#   ./manage-stream-amd64.sh stop   --port 18318
#   ./manage-stream-amd64.sh stop   --all
#   ./manage-stream-amd64.sh status --port 18318
#   ./manage-stream-amd64.sh status --all
#   ./manage-stream-amd64.sh list
#   ./manage-stream-amd64.sh log    --port 18318

APP_NAME="stream-converter-linux-amd64"

# 获取脚本所在目录
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR" || exit 1

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# ─── 实例标识 ──────────────────────────────────────────────
# 如果指定了 --port，使用 port 作为实例标识；否则用默认值 18318
INSTANCE_PORT="${PORT:-18318}"

# 实例相关的文件路径
instance_pid_file()   { echo "$SCRIPT_DIR/${APP_NAME}-${1}.pid";   }
instance_log_file()   { echo "$SCRIPT_DIR/${APP_NAME}-${1}.log";   }
instance_lock_file()  { echo "$SCRIPT_DIR/${APP_NAME}-${1}.lock";  }

# ─── 检查单个实例是否在运行 ────────────────────────────────
check_status() {
    local port="$1"
    local pid_file
    pid_file="$(instance_pid_file "$port")"

    if [ -f "$pid_file" ]; then
        local pid
        pid=$(cat "$pid_file")
        if ps -p "$pid" > /dev/null 2>&1; then
            # 进一步确认进程名是否匹配，避免 pid 被重用
            local cmd_line
            cmd_line=$(ps -p "$pid" -o cmd= 2>/dev/null)
            if echo "$cmd_line" | grep -q "$APP_NAME"; then
                return 0  # 正在运行
            fi
        fi
        # pid 文件无效，清理
        rm -f "$pid_file"
    fi
    return 1  # 未运行
}

# ─── 获取所有运行中的实例端口列表 ───────────────────────────
list_running_instances() {
    local ports=""
    for pid_file in "$SCRIPT_DIR/${APP_NAME}-"*.pid; do
        [ -f "$pid_file" ] || continue
        local port
        port=$(basename "$pid_file" | sed "s/${APP_NAME}-//" | sed 's/\.pid$//')
        if check_status "$port"; then
            ports="$ports $port"
        else
            rm -f "$pid_file"
        fi
    done
    echo "$ports"
}

# ─── 获取所有已注册（有 pid 文件）的实例端口列表 ──────────
list_all_instances() {
    local ports=""
    for pid_file in "$SCRIPT_DIR/${APP_NAME}-"*.pid; do
        [ -f "$pid_file" ] || continue
        local port
        port=$(basename "$pid_file" | sed "s/${APP_NAME}-//" | sed 's/\.pid$//')
        ports="$ports $port"
    done
    echo "$ports"
}

# ─── 提取上游配置（从进程命令行中提取） ─────────────────────
extract_upstream() {
    local port="$1"
    local pid_file
    pid_file="$(instance_pid_file "$port")"

    if [ ! -f "$pid_file" ]; then
        echo "-"
        return
    fi
    local pid
    pid=$(cat "$pid_file")
    if ! ps -p "$pid" > /dev/null 2>&1; then
        echo "-"
        return
    fi
    local cmd_line
    cmd_line=$(ps -p "$pid" -o cmd= 2>/dev/null)
    # 尝试从环境变量 /proc/PID/environ 中读取 UPSTREAM_URL（更可靠）
    if [ -r "/proc/${pid}/environ" ]; then
        local upstream
        upstream=$(tr '\0' '\n' < "/proc/${pid}/environ" 2>/dev/null | grep '^UPSTREAM_URL=' | cut -d= -f2-)
        if [ -n "$upstream" ]; then
            echo "$upstream"
            return
        fi
    fi
    # fallback: 从命令行参数中提取
    if echo "$cmd_line" | grep -q -- '--upstream-url'; then
        echo "$cmd_line" | sed 's/.*--upstream-url\s\+//' | awk '{print $1}'
    else
        echo "-"
    fi
}

# ─── 提取端口配置（从进程命令行中提取） ─────────────────────
extract_port() {
    local port="$1"
    local pid_file
    pid_file="$(instance_pid_file "$port")"

    if [ ! -f "$pid_file" ]; then
        echo "-"
        return
    fi
    local pid
    pid=$(cat "$pid_file")
    if ! ps -p "$pid" > /dev/null 2>&1; then
        echo "-"
        return
    fi
    local cmd_line
    cmd_line=$(ps -p "$pid" -o cmd= 2>/dev/null)
    if [ -r "/proc/${pid}/environ" ]; then
        local env_port
        env_port=$(tr '\0' '\n' < "/proc/${pid}/environ" 2>/dev/null | grep '^PORT=' | cut -d= -f2-)
        if [ -n "$env_port" ]; then
            echo "$env_port"
            return
        fi
    fi
    echo "$port"
}

# ─── 启动单个实例 ──────────────────────────────────────────
start() {
    local port="$1"
    local upstream="$2"
    local timeout="${TIMEOUT:-600}"
    local debug="${DEBUG:-false}"

    local pid_file log_file lock_file
    pid_file="$(instance_pid_file "$port")"
    log_file="$(instance_log_file "$port")"
    lock_file="$(instance_lock_file "$port")"

    # 并发锁 — 防止同时启动同一个实例
    exec 9>"$lock_file"
    if ! flock -n 9; then
        printf '%b\n' "${RED}[错误]${NC} 实例 :$port 已有启动操作在运行 (锁文件: $lock_file)"
        exit 1
    fi

    if check_status "$port"; then
        printf '%b\n' "${YELLOW}[警告]${NC} 实例 :$port 已经在运行中"
        return 0
    fi

    printf '%b\n' "${GREEN}[启动]${NC} 正在启动实例 :$port → $upstream ..."

    # 检查二进制文件是否存在
    if [ ! -f "$APP_NAME" ]; then
        printf '%b\n' "${RED}[错误]${NC} 找不到二进制文件: $APP_NAME"
        exit 1
    fi

    # 检查二进制文件是否有执行权限
    if [ ! -x "$APP_NAME" ]; then
        printf '%b\n' "${YELLOW}[警告]${NC} 添加执行权限: $APP_NAME"
        chmod +x "$APP_NAME"
    fi

    # 用环境变量传递配置
    PORT="$port" \
    UPSTREAM_URL="$upstream" \
    TIMEOUT="$timeout" \
    DEBUG="$debug" \
    nohup "./$APP_NAME" >> "$log_file" 2>&1 &
    local pid=$!

    # 保存 PID
    echo "$pid" > "$pid_file"

    # 等待一下确认进程是否成功启动
    sleep 2

    if ps -p "$pid" > /dev/null 2>&1; then
        printf '%b\n' "${GREEN}[成功]${NC} 实例 :$port 已启动 (PID: $pid, 上游: $upstream)"
        printf '%b\n' "${GREEN}[信息]${NC} 日志文件: $log_file"
    else
        printf '%b\n' "${RED}[失败]${NC} 实例 :$port 启动失败，请检查日志"
        rm -f "$pid_file"
        exit 1
    fi
}

# ─── 停止单个实例 ──────────────────────────────────────────
stop_instance() {
    local port="$1"
    local pid_file log_file
    pid_file="$(instance_pid_file "$port")"
    log_file="$(instance_log_file "$port")"

    if ! check_status "$port"; then
        printf '%b\n' "${YELLOW}[警告]${NC} 实例 :$port 当前未在运行"
        # 尝试通过路径精确清理残留进程
        pkill -f "^$SCRIPT_DIR/$APP_NAME.*PORT=$port" 2>/dev/null || true
        pkill -f "^$SCRIPT_DIR/$APP_NAME.*--port $port" 2>/dev/null || true
        rm -f "$pid_file"
        return 0
    fi

    local pid
    pid=$(cat "$pid_file")

    # 校验 PID 是否为有效数字
    case "$pid" in
        *[!0-9]*)
            printf '%b\n' "${RED}[错误]${NC} 实例 :$port PID 文件内容异常: '$pid'，清理后退出"
            rm -f "$pid_file"
            exit 1
            ;;
    esac

    printf '%b\n' "${GREEN}[停止]${NC} 正在停止实例 :$port (PID: $pid) ..."

    # 先尝试优雅终止
    kill "$pid" 2>/dev/null

    # 等待进程结束
    local waited=0
    while [ "$waited" -lt 10 ]; do
        if ! ps -p "$pid" > /dev/null 2>&1; then
            printf '%b\n' "${GREEN}[成功]${NC} 实例 :$port 已停止"
            rm -f "$pid_file"
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    # 如果优雅终止失败，强制终止
    printf '%b\n' "${YELLOW}[警告]${NC} 实例 :$port 未响应，正在强制终止 ..."
    kill -9 "$pid" 2>/dev/null

    if [ $? -eq 0 ]; then
        printf '%b\n' "${GREEN}[成功]${NC} 实例 :$port 已强制停止"
    else
        printf '%b\n' "${RED}[失败]${NC} 无法停止实例 :$port"
    fi

    rm -f "$pid_file"
}

# ─── 停止所有实例 ──────────────────────────────────────────
stop_all() {
    local ports
    ports=$(list_all_instances)
    if [ -z "$ports" ]; then
        printf '%b\n' "${YELLOW}[信息]${NC} 没有运行中的实例"
        return 0
    fi

    printf '%b\n' "${GREEN}[停止]${NC} 正在停止所有实例 ..."
    local has_error=0
    for port in $ports; do
        stop_instance "$port"
        if [ $? -ne 0 ]; then
            has_error=1
        fi
    done

    if [ "$has_error" -eq 0 ]; then
        printf '%b\n' "${GREEN}[完成]${NC} 所有实例已停止"
    else
        printf '%b\n' "${YELLOW}[完成]${NC} 部分实例停止时出现错误"
    fi
}

# ─── 查看单个实例状态 ──────────────────────────────────────
status_instance() {
    local port="$1"
    local pid_file log_file
    pid_file="$(instance_pid_file "$port")"
    log_file="$(instance_log_file "$port")"

    if check_status "$port"; then
        local pid
        pid=$(cat "$pid_file")
        local actual_port
        actual_port=$(extract_port "$port")
        local upstream
        upstream=$(extract_upstream "$port")

        printf '%b\n' "${GREEN}[运行中]${NC} 实例 :$actual_port (PID: $pid)"
        printf '%b\n' "  上游地址: $upstream"
        printf '%b\n' "  日志文件: $log_file"

        # 显示进程详细信息
        printf '%b\n' "  进程详情:"
        ps -p "$pid" -o pid,ppid,cmd,%cpu,%mem,etime 2>/dev/null | sed 's/^/    /'

        # 显示监听端口
        local netstat_cmd
        netstat_cmd=$(command -v netstat 2>/dev/null)
        local ss_cmd
        ss_cmd=$(command -v ss 2>/dev/null)
        if [ -n "$netstat_cmd" ]; then
            printf '%b\n' "  网络连接:"
            $netstat_cmd -tulnp 2>/dev/null | grep "$pid" | sed 's/^/    /'
        elif [ -n "$ss_cmd" ]; then
            printf '%b\n' "  网络连接:"
            $ss_cmd -tulnp 2>/dev/null | grep "$pid" | sed 's/^/    /'
        fi
    else
        printf '%b\n' "${RED}[已停止]${NC} 实例 :$port"
    fi
}

# ─── 查看所有实例状态 ──────────────────────────────────────
status_all() {
    local ports
    ports=$(list_all_instances)
    if [ -z "$ports" ]; then
        # 也尝试扫描可能被 pkill 但 pid 文件已丢的残留进程
        local remaining
        remaining=$(ps aux 2>/dev/null | grep "$APP_NAME" | grep -v grep | awk '{print $2}')
        if [ -n "$remaining" ]; then
            printf '%b\n' "${YELLOW}[信息]${NC} 没有通过 pid 文件管理的实例，但发现残留进程:"
            ps -p "$remaining" -o pid,cmd 2>/dev/null | sed 's/^/  /'
            printf '%b\n' "${YELLOW}[提示]${NC} 可使用 'pkill -f $APP_NAME' 清理"
        else
            printf '%b\n' "${YELLOW}[信息]${NC} 当前没有运行中的实例"
        fi
        return 0
    fi

    printf '%b\n' "${BLUE}========================================${NC}"
    printf '%b\n' "${BLUE}  Stream Converter 实例列表${NC}"
    printf '%b\n' "${BLUE}========================================${NC}"
    local running_count=0
    local stopped_count=0
    for port in $ports; do
        if check_status "$port"; then
            running_count=$((running_count + 1))
        else
            stopped_count=$((stopped_count + 1))
        fi
        echo ""
        status_instance "$port"
    done
    echo ""
    printf '%b\n' "${BLUE}----------------------------------------${NC}"
    printf '%b\n' "总计: $((running_count + stopped_count)) 个实例"
    printf '%b\n' "${GREEN}运行中: $running_count${NC}"
    printf '%b\n' "${RED}已停止: $stopped_count${NC}"
    printf '%b\n' "${BLUE}========================================${NC}"
}

# ─── 列出所有实例（简洁版） ─────────────────────────────────
list_instances() {
    local ports
    ports=$(list_all_instances)
    if [ -z "$ports" ]; then
        printf '%b\n' "${YELLOW}[信息]${NC} 没有已注册的实例"
        return 0
    fi

    printf "%-10s %-10s %-30s %s\n" "端口" "状态" "上游地址" "日志文件"
    printf "%-10s %-10s %-30s %s\n" "----" "----" "--------" "--------"
    for port in $ports; do
        local status_text pid
        if check_status "$port"; then
            pid=$(cat "$(instance_pid_file "$port")")
            status_text="${GREEN}运行中${NC} (PID $pid)"
        else
            status_text="${RED}已停止${NC}"
        fi
        local upstream
        upstream=$(extract_upstream "$port")
        local log_file
        log_file="$(instance_log_file "$port")"
        printf "%-10s %-20s %-30s %s\n" "$port" "$(printf '%b' "$status_text")" "$upstream" "$log_file"
    done
}

# ─── 查看日志 ──────────────────────────────────────────────
log_instance() {
    local port="$1"
    local log_file
    log_file="$(instance_log_file "$port")"

    if [ ! -f "$log_file" ]; then
        printf '%b\n' "${YELLOW}[警告]${NC} 实例 :$port 日志文件不存在: $log_file"
        exit 0
    fi

    printf '%b\n' "${GREEN}[日志]${NC} 显示实例 :$port 日志 (最后 50 行，按 Ctrl+C 退出)\n"
    tail -n 50 -f "$log_file"
}

# ─── 重启单个实例 ──────────────────────────────────────────
restart_instance() {
    local port="$1"
    local upstream="$2"

    printf '%b\n' "${GREEN}[重启]${NC} 正在重启实例 :$port ..."
    stop_instance "$port"
    sleep 2
    start "$port" "$upstream"
}

# ─── 重启所有实例 ──────────────────────────────────────────
restart_all() {
    local ports
    ports=$(list_all_instances)
    if [ -z "$ports" ]; then
        printf '%b\n' "${YELLOW}[信息]${NC} 没有已注册的实例"
        return 0
    fi

    printf '%b\n' "${GREEN}[重启]${NC} 正在重启所有实例 ..."
    # 先全部停止
    for port in $ports; do
        stop_instance "$port"
    done
    sleep 2
    # 再全部启动（使用之前记录的配置）
    for port in $ports; do
        local upstream
        upstream=$(extract_upstream "$port")
        if [ "$upstream" = "-" ]; then
            printf '%b\n' "${YELLOW}[警告]${NC} 实例 :$port 无法获取上游地址，跳过启动"
            continue
        fi
        start "$port" "$upstream"
    done
}

# ─── 显示帮助信息 ──────────────────────────────────────────
show_help() {
    echo ""
    echo "Stream Converter (AMD64 Binary) 多实例管理脚本"
    echo "================================================="
    echo "支持同时运行多个实例，不同端口对应不同上游地址。"
    echo ""
    echo "用法:"
    echo "  $0 <命令> [选项]"
    echo ""
    echo "命令:"
    echo "  start     启动一个实例（必须指定 --port 和 --upstream-url）"
    echo "  stop      停止指定实例    (--port PORT) 或停止所有实例 (--all)"
    echo "  restart   重启指定实例    (--port PORT) 或重启所有实例 (--all)"
    echo "  status    查看指定实例状态 (--port PORT) 或查看所有实例 (--all)"
    echo "  log       查看实例日志     (--port PORT)"
    echo "  list      列出所有已注册的实例及状态"
    echo ""
    echo "选项:"
    echo "  -h, --help           显示帮助信息"
    echo "  --port PORT          监听端口号 (默认: 18318)"
    echo "  --upstream-url URL   上游 URL (启动时必填)"
    echo "  --timeout SECONDS    超时时间秒 (默认: 600)"
    echo "  --debug              启用调试模式"
    echo "  --all                对所有实例执行操作 (仅用于 stop/restart/status)"
    echo ""
    echo "环境变量 (优先级低于 -- 选项):"
    echo "  PORT             监听端口"
    echo "  UPSTREAM_URL     上游 URL"
    echo "  TIMEOUT          超时时间秒"
    echo "  DEBUG            调试模式"
    echo ""
    echo "示例:"
    echo "  # 启动多个实例"
    echo "  $0 start --port 18318 --upstream-url http://127.0.0.1:8317"
    echo "  $0 start --port 18319 --upstream-url http://127.0.0.1:9000"
    echo ""
    echo "  # 实例管理"
    echo "  $0 status --port 18318"
    echo "  $0 status --all"
    echo "  $0 list"
    echo "  $0 log --port 18318"
    echo "  $0 stop --port 18318"
    echo "  $0 stop --all"
    echo "  $0 restart --port 18318"
    echo ""
    echo "  # 使用环境变量"
    echo "  PORT=18320 UPSTREAM_URL=http://127.0.0.1:8080 $0 start"
    echo ""
}

# ══════════════════════════════════════════════════════════
#  解析命令行参数
# ══════════════════════════════════════════════════════════

PARSED_CMD=""
INSTANCE_SCOPE=""   # "" = 未指定, "port" = --port, "all" = --all
CLI_PORT=""
CLI_UPSTREAM=""
CLI_TIMEOUT=""
CLI_DEBUG=""

while [ $# -gt 0 ]; do
    case "$1" in
        --port)
            if [ -z "$2" ] || [ "${2#--}" != "$2" ]; then
                printf '%b\n' "${RED}[错误]${NC} --port 需要指定端口号" >&2
                exit 1
            fi
            CLI_PORT="$2"
            INSTANCE_SCOPE="port"
            shift 2
            ;;
        --upstream-url)
            if [ -z "$2" ] || [ "${2#--}" != "$2" ]; then
                printf '%b\n' "${RED}[错误]${NC} --upstream-url 需要指定 URL" >&2
                exit 1
            fi
            CLI_UPSTREAM="$2"
            shift 2
            ;;
        --timeout)
            if [ -z "$2" ] || [ "${2#--}" != "$2" ]; then
                printf '%b\n' "${RED}[错误]${NC} --timeout 需要指定秒数" >&2
                exit 1
            fi
            CLI_TIMEOUT="$2"
            shift 2
            ;;
        --debug)
            CLI_DEBUG="true"
            shift
            ;;
        --all)
            INSTANCE_SCOPE="all"
            shift
            ;;
        -h|--help|help)
            show_help
            exit 0
            ;;
        start|stop|restart|status|log|list)
            PARSED_CMD="$1"
            shift
            ;;
        *)
            printf '%b\n' "${RED}[错误]${NC} 未知选项: $1" >&2
            show_help
            exit 1
            ;;
    esac
done

# ─── 合并配置优先级: CLI 选项 > 环境变量 > 默认值 ─────────
PORT="${CLI_PORT:-${PORT:-18318}}"
UPSTREAM_URL="${CLI_UPSTREAM:-${UPSTREAM_URL:-}}"
TIMEOUT="${CLI_TIMEOUT:-${TIMEOUT:-600}}"
DEBUG="${CLI_DEBUG:-${DEBUG:-false}}"

# ══════════════════════════════════════════════════════════
#  主逻辑
# ══════════════════════════════════════════════════════════

case "$PARSED_CMD" in
    start)
        if [ -z "$UPSTREAM_URL" ]; then
            printf '%b\n' "${RED}[错误]${NC} start 命令必须指定 --upstream-url" >&2
            show_help
            exit 1
        fi
        start "$PORT" "$UPSTREAM_URL"
        ;;
    stop)
        if [ "$INSTANCE_SCOPE" = "all" ]; then
            stop_all
        elif [ "$INSTANCE_SCOPE" = "port" ]; then
            stop_instance "$PORT"
        else
            printf '%b\n' "${RED}[错误]${NC} stop 需要指定 --port PORT 或 --all" >&2
            show_help
            exit 1
        fi
        ;;
    restart)
        if [ "$INSTANCE_SCOPE" = "all" ]; then
            restart_all
        elif [ "$INSTANCE_SCOPE" = "port" ]; then
            if [ -z "$UPSTREAM_URL" ]; then
                # 尝试从运行中的进程获取 upstream
                local saved_upstream
                saved_upstream=$(extract_upstream "$PORT")
                if [ "$saved_upstream" = "-" ]; then
                    printf '%b\n' "${RED}[错误]${NC} 无法获取实例 :$PORT 的上游地址，请显式指定 --upstream-url" >&2
                    exit 1
                fi
                UPSTREAM_URL="$saved_upstream"
            fi
            restart_instance "$PORT" "$UPSTREAM_URL"
        else
            printf '%b\n' "${RED}[错误]${NC} restart 需要指定 --port PORT 或 --all" >&2
            show_help
            exit 1
        fi
        ;;
    status)
        if [ "$INSTANCE_SCOPE" = "all" ]; then
            status_all
        elif [ "$INSTANCE_SCOPE" = "port" ]; then
            status_instance "$PORT"
        else
            status_all
        fi
        ;;
    log)
        if [ "$INSTANCE_SCOPE" != "port" ]; then
            printf '%b\n' "${RED}[错误]${NC} log 命令需要指定 --port PORT" >&2
            show_help
            exit 1
        fi
        log_instance "$PORT"
        ;;
    list)
        list_instances
        ;;
    *)
        show_help
        exit 1
        ;;
esac
