#!/bin/bash
# 未定义变量即报错，避免因变量拼写错误导致静默异常
set -u

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
#   ./manage-stream-amd64.sh log    --port 18318 [--lines 100]

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

# ─── 实例相关的文件路径 ────────────────────────────────────
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

# ─── 读取运行中实例的完整环境变量（多行 KEY=VALUE） ──────
# 实例未运行、无 pid 文件或 /proc 不可读时返回空
instance_environ() {
    local port="$1"
    local pid_file pid
    pid_file="$(instance_pid_file "$port")"
    [ -f "$pid_file" ] || return 0
    pid=$(cat "$pid_file")
    if ! ps -p "$pid" > /dev/null 2>&1; then
        return 0
    fi
    if [ ! -r "/proc/${pid}/environ" ]; then
        return 0
    fi
    tr '\0' '\n' < "/proc/${pid}/environ" 2>/dev/null
}

# ─── 从 "KEY=VALUE" 多行文本中提取指定 key 的值 ───────────
env_value_of() {
    local envs="$1" key="$2"
    local line k
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        k="${line%%=*}"
        if [ "$k" = "$key" ]; then
            echo "${line#*=}"
            return 0
        fi
    done <<< "$envs"
    return 1
}

# ─── 从已读取的环境变量文本生成缓存摘要 ───────────────────
cache_summary_of_envs() {
    local envs="$1"
    [ -n "$envs" ] || { echo "-"; return; }
    local enabled ttl entries
    enabled=$(env_value_of "$envs" CACHE_ENABLED)
    ttl=$(env_value_of "$envs" CACHE_TTL_SECS)
    entries=$(env_value_of "$envs" CACHE_MAX_ENTRIES)
    if [ "$enabled" = "true" ]; then
        echo "ON (${ttl}s / ${entries}条)"
    else
        echo "OFF"
    fi
}

# ─── 提取上游配置 ──────────────────────────────────────────
extract_upstream() {
    local port="$1"
    local envs upstream
    envs=$(instance_environ "$port")
    [ -n "$envs" ] || { echo "-"; return; }
    upstream=$(env_value_of "$envs" UPSTREAM_URL)
    if [ -n "$upstream" ]; then
        echo "$upstream"
    else
        echo "-"
    fi
}

# ─── 提取端口配置 ──────────────────────────────────────────
extract_port() {
    local port="$1"
    local envs env_port
    envs=$(instance_environ "$port")
    [ -n "$envs" ] || { echo "$port"; return; }
    env_port=$(env_value_of "$envs" PORT)
    if [ -n "$env_port" ]; then
        echo "$env_port"
    else
        echo "$port"
    fi
}

# ─── 提取缓存命中折扣配置 ──────────────────────────────────
extract_cache_hit_discount() {
    local port="$1"
    local envs discount
    envs=$(instance_environ "$port")
    [ -n "$envs" ] || { echo "-"; return; }
    discount=$(env_value_of "$envs" CACHE_HIT_DISCOUNT)
    if [ -n "$discount" ]; then
        echo "$discount"
    else
        echo "-"
    fi
}

# ─── 提取缓存相关配置（多行 KEY=VALUE，含 CACHE_HIT_DISCOUNT）──
extract_cache_envs() {
    local port="$1"
    local envs
    envs=$(instance_environ "$port")
    [ -n "$envs" ] || return 0
    printf '%s\n' "$envs" \
        | grep -E '^(CACHE_ENABLED|CACHE_TTL_SECS|CACHE_MAX_ENTRIES|CACHE_MAX_RESPONSE_BYTES|CACHE_HIT_DISCOUNT)=' \
        || true
}

# ─── 生成缓存配置摘要（用于 status/list 展示） ─────────────
extract_cache_summary() {
    local port="$1"
    local envs
    envs=$(instance_environ "$port")
    cache_summary_of_envs "$envs"
}

# ─── 从多行 "KEY=VALUE" 文本恢复缓存环境变量到当前 shell ──
# 仅当当前 shell 中对应变量为空时才恢复，避免覆盖用户显式指定的值
apply_cache_envs() {
    local envs="$1"
    [ -n "$envs" ] || return 0
    local line key val
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        key="${line%%=*}"
        val="${line#*=}"
        case "$key" in
            CACHE_ENABLED|CACHE_TTL_SECS|CACHE_MAX_ENTRIES|CACHE_MAX_RESPONSE_BYTES|CACHE_HIT_DISCOUNT)
                # 使用 ${var:-} 形式，兼容 set -u（变量可能尚未定义）
                if eval "[ -z \"\${$key:-}\" ]" && [ -n "$val" ]; then
                    export "$key=$val"
                fi
                ;;
        esac
    done <<< "$envs"
}

# ─── 恢复实例的缓存配置到当前 shell（供 restart 使用） ────
# 注意：必须在 stop 之前调用（stop 会删除 pid 文件，进程退出后无法从 /proc 读取）
restore_cache_envs() {
    local port="$1"
    local envs
    envs=$(extract_cache_envs "$port")
    apply_cache_envs "$envs"
}

# ─── 启动单个实例 ──────────────────────────────────────────
start() {
    local port="$1"
    local upstream="$2"
    local timeout="${TIMEOUT:-600}"
    local debug="${DEBUG:-false}"
    local cache_hit_discount="${CACHE_HIT_DISCOUNT:-}"
    local cache_enabled="${CACHE_ENABLED:-false}"
    local cache_ttl_secs="${CACHE_TTL_SECS:-300}"
    local cache_max_entries="${CACHE_MAX_ENTRIES:-100}"
    local cache_max_response_bytes="${CACHE_MAX_RESPONSE_BYTES:-102400}"

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
    CACHE_HIT_DISCOUNT="$cache_hit_discount" \
    CACHE_ENABLED="$cache_enabled" \
    CACHE_TTL_SECS="$cache_ttl_secs" \
    CACHE_MAX_ENTRIES="$cache_max_entries" \
    CACHE_MAX_RESPONSE_BYTES="$cache_max_response_bytes" \
    nohup "./$APP_NAME" >> "$log_file" 2>&1 &
    local pid=$!

    # 保存 PID
    echo "$pid" > "$pid_file"

    # 等待一下确认进程是否成功启动
    sleep 2

    if ps -p "$pid" > /dev/null 2>&1; then
        local discount_info=""
        if [ -n "$cache_hit_discount" ]; then
            discount_info=", 折扣: ×${cache_hit_discount}"
        fi
        local cache_info=""
        if [ "$cache_enabled" = "true" ]; then
            cache_info=", 缓存: 开启(TTL=${cache_ttl_secs}s/${cache_max_entries}条)"
        else
            cache_info=", 缓存: 关闭"
        fi
        printf '%b\n' "${GREEN}[成功]${NC} 实例 :$port 已启动 (PID: $pid, 上游: $upstream${discount_info}${cache_info})"
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
        # 一次读取环境变量，避免重复访问 /proc
        local envs
        envs=$(instance_environ "$port")

        local actual_port
        actual_port=$(env_value_of "$envs" PORT)
        [ -n "$actual_port" ] || actual_port="$port"

        local upstream
        upstream=$(env_value_of "$envs" UPSTREAM_URL)
        [ -n "$upstream" ] || upstream="-"

        local discount
        discount=$(env_value_of "$envs" CACHE_HIT_DISCOUNT)
        [ -n "$discount" ] || discount="-"

        local cache_summary
        cache_summary=$(cache_summary_of_envs "$envs")

        printf '%b\n' "${GREEN}[运行中]${NC} 实例 :$actual_port (PID: $pid)"
        printf '%b\n' "  上游地址: $upstream"
        if [ "$cache_summary" != "-" ]; then
            printf '%b\n' "  响应缓存:  ${cache_summary}"
        fi
        if [ "$discount" != "-" ]; then
            printf '%b\n' "  缓存折扣:    ×${discount}"
        fi
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

    printf "%-10s %-10s %-30s %-18s %-10s %s\n" "端口" "状态" "上游地址" "响应缓存" "缓存折扣" "日志文件"
    printf "%-10s %-10s %-30s %-18s %-10s %s\n" "----" "----" "--------" "--------" "--------" "--------"
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
        local cache_summary
        cache_summary=$(extract_cache_summary "$port")
        local discount
        discount=$(extract_cache_hit_discount "$port")
        local discount_text
        if [ "$discount" = "-" ]; then
            discount_text="-"
        else
            discount_text="×${discount}"
        fi
        local log_file
        log_file="$(instance_log_file "$port")"
        printf "%-10s %-20s %-30s %-18s %-10s %s\n" "$port" "$(printf '%b' "$status_text")" "$upstream" "$cache_summary" "$discount_text" "$log_file"
    done
}

# ─── 查看日志 ──────────────────────────────────────────────
log_instance() {
    local port="$1"
    local lines="$2"
    local log_file
    log_file="$(instance_log_file "$port")"

    if [ ! -f "$log_file" ]; then
        printf '%b\n' "${YELLOW}[警告]${NC} 实例 :$port 日志文件不存在: $log_file"
        exit 0
    fi

    printf '%b\n' "${GREEN}[日志]${NC} 显示实例 :$port 日志 (最后 ${lines} 行，按 Ctrl+C 退出)\n"
    tail -n "$lines" -f "$log_file"
}

# ─── 重启单个实例 ──────────────────────────────────────────
restart_instance() {
    local port="$1"
    local upstream="$2"
    local discount="$3"

    printf '%b\n' "${GREEN}[重启]${NC} 正在重启实例 :$port ..."
    # 在 stop 前恢复该实例的缓存相关配置（stop 后进程退出，无法再从 /proc 读取）
    restore_cache_envs "$port"
    stop_instance "$port"
    sleep 2
    # 在 start 前设置全局 CACHE_HIT_DISCOUNT（向后兼容旧参数传递）
    if [ -n "$discount" ] && [ "$discount" != "-" ]; then
        CACHE_HIT_DISCOUNT="$discount"
    fi
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

    # 在停止前保存每个实例的上游地址与缓存配置
    # （stop 会删除 pid 文件且进程退出，之后再无法从 /proc 读取）
    declare -A saved_upstreams
    declare -A saved_cache_envs
    for port in $ports; do
        saved_upstreams[$port]=$(extract_upstream "$port")
        saved_cache_envs[$port]=$(extract_cache_envs "$port")
    done

    # 捕获 CLI 或环境变量文件显式指定的缓存配置（unset 前保存，供启动时统一应用）
    local exp_cache_enabled=""
    is_explicitly_set CACHE_ENABLED && exp_cache_enabled="$CACHE_ENABLED"
    local exp_cache_ttl_secs=""
    is_explicitly_set CACHE_TTL_SECS && exp_cache_ttl_secs="$CACHE_TTL_SECS"
    local exp_cache_max_entries=""
    is_explicitly_set CACHE_MAX_ENTRIES && exp_cache_max_entries="$CACHE_MAX_ENTRIES"
    local exp_cache_max_bytes=""
    is_explicitly_set CACHE_MAX_RESPONSE_BYTES && exp_cache_max_bytes="$CACHE_MAX_RESPONSE_BYTES"
    local exp_discount=""
    is_explicitly_set CACHE_HIT_DISCOUNT && exp_discount="$CACHE_HIT_DISCOUNT"

    # 先全部停止
    for port in $ports; do
        stop_instance "$port"
    done
    sleep 2

    # 再全部启动（使用之前记录的配置）
    for port in $ports; do
        local upstream
        upstream="${saved_upstreams[$port]}"
        if [ -z "$upstream" ] || [ "$upstream" = "-" ]; then
            printf '%b\n' "${YELLOW}[警告]${NC} 实例 :$port 无法获取上游地址，跳过启动"
            continue
        fi
        # 清理上一实例残留的缓存环境变量，避免配置串扰
        unset CACHE_ENABLED CACHE_TTL_SECS CACHE_MAX_ENTRIES CACHE_MAX_RESPONSE_BYTES CACHE_HIT_DISCOUNT 2>/dev/null || true
        # 恢复 CLI/env 文件显式指定的配置（apply_cache_envs 只补缺失项，不覆盖显式值）
        [ -n "$exp_cache_enabled" ] && CACHE_ENABLED="$exp_cache_enabled"
        [ -n "$exp_cache_ttl_secs" ] && CACHE_TTL_SECS="$exp_cache_ttl_secs"
        [ -n "$exp_cache_max_entries" ] && CACHE_MAX_ENTRIES="$exp_cache_max_entries"
        [ -n "$exp_cache_max_bytes" ] && CACHE_MAX_RESPONSE_BYTES="$exp_cache_max_bytes"
        [ -n "$exp_discount" ] && CACHE_HIT_DISCOUNT="$exp_discount"
        apply_cache_envs "${saved_cache_envs[$port]}"
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
    echo "  start     启动一个实例（--upstream-url 必填，可用 -f env 文件提供）"
    echo "  stop      停止指定实例    (--port PORT) 或停止所有实例 (--all)"
    echo "  restart   重启指定实例    (--port PORT) 或重启所有实例 (--all)"
    echo "  status    查看指定实例状态 (--port PORT) 或查看所有实例 (--all)"
    echo "  log       查看实例日志     (--port PORT)，可用 --lines 指定行数"
    echo "  list      列出所有已注册的实例及状态"
    echo ""
    echo "选项:"
    echo "  -h, --help           显示帮助信息"
    echo "  -f, --env-file FILE  从环境变量文件加载配置 (类似 docker --env-file)"
    echo "                       env 文件含 PORT 时，start/stop/restart/status/log 均可省略 --port"
    echo "  --port PORT          监听端口号 (默认: 18318)"
    echo "  --upstream-url URL   上游 URL (启动时必填)"
    echo "  --timeout SECONDS        超时时间秒 (默认: 600)"
    echo "  --debug                  启用调试模式"
    echo "  --cache-hit-discount RATIO 缓存命中数折扣比例 (如 0.5 即减半)"
    echo "  --cache-enabled BOOL     响应缓存开关 true/false (默认: false)"
    echo "  --cache-ttl SECONDS      缓存条目 TTL 秒 (默认: 300)"
    echo "  --cache-max-entries N    最大缓存条目数 (默认: 100)"
    echo "  --cache-max-response-bytes N 单响应最大字节数 (默认: 102400, 即 100KB)"
    echo "  --lines N                 查看日志行数 (默认: 50)"
    echo "  --all                    对所有实例执行操作 (仅用于 stop/restart/status)"
    echo ""
    echo "环境变量 (优先级低于 -- 选项):"
    echo "  PORT                 监听端口"
    echo "  UPSTREAM_URL         上游 URL"
    echo "  TIMEOUT              超时时间秒"
    echo "  DEBUG                调试模式"
    echo "  CACHE_HIT_DISCOUNT   缓存命中数折扣比例"
    echo "  CACHE_ENABLED        响应缓存开关 true/false"
    echo "  CACHE_TTL_SECS       缓存条目 TTL 秒"
    echo "  CACHE_MAX_ENTRIES    最大缓存条目数"
    echo "  CACHE_MAX_RESPONSE_BYTES 单响应最大字节数"
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
    echo "  $0 log --port 18318 --lines 200"
    echo "  $0 stop --port 18318"
    echo "  $0 stop --all"
    echo "  $0 restart --port 18318"
    echo ""
    echo "  # 使用环境变量"
    echo "  PORT=18320 UPSTREAM_URL=http://127.0.0.1:8080 $0 start"
    echo ""
    echo "  # 使用环境变量文件 (docker --env-file 风格)"
    echo "  $0 start --upstream-url http://127.0.0.1:8080 -f my.env"
    echo ""
}

# ─── 加载环境变量文件（类似 docker --env-file） ───────────
# 规则:
#   - 忽略空行与以 # 开头的行
#   - 支持可选 export 前缀 (export KEY=VALUE)
#   - 仅接受 KEY=VALUE 形式，KEY 必须是合法变量名
#   - 值可带单引号/双引号（解析时去掉最外层引号）
#   - 文件中出现的变量会覆盖当前 shell 环境变量
#     （但优先级仍低于 CLI 选项，CLI > env文件 > 环境变量 > 默认值）
load_env_file() {
    local file="$1"
    if [ ! -f "$file" ]; then
        printf '%b\n' "${RED}[错误]${NC} 环境变量文件不存在: $file" >&2
        exit 1
    fi

    local line key val
    while IFS= read -r line || [ -n "$line" ]; do
        # 去掉行首空白
        line="${line#"${line%%[![:space:]]*}"}"
        # 忽略空行与注释
        case "$line" in
            '' | '#'*) continue ;;
        esac
        # 去掉可选 export 前缀
        if [ "${line#export }" != "$line" ]; then
            line="${line#export }"
            line="${line#"${line%%[![:space:]]*}"}"
        fi
        # 提取 KEY
        key="${line%%=*}"
        if [ -z "$key" ] || [ "$key" = "$line" ]; then
            printf '%b\n' "${YELLOW}[警告]${NC} 忽略无效行: $line" >&2
            continue
        fi
        # 去掉 key 尾部空白并校验变量名合法性
        key="${key%"${key##*[![:space:]]}"}"
        if ! printf '%s' "$key" | grep -qE '^[a-zA-Z_][a-zA-Z0-9_]*$'; then
            printf '%b\n' "${YELLOW}[警告]${NC} 忽略非法变量名: $key" >&2
            continue
        fi
        # 提取 VALUE（去掉最外层引号）
        val="${line#*=}"
        case "$val" in
            '"'*'"') val="${val#\"}"; val="${val%\"}" ;;
            "'"*"'") val="${val#\'}"; val="${val%\'}" ;;
        esac
        export "$key=$val"
        # 记录变量来源，用于 restart 时判断"是否显式指定"
        # （显式指定的变量在 restart 时优先于实例保存的旧配置）
        case " $ENV_FILE_KEYS " in
            *" $key "*) ;;
            *) ENV_FILE_KEYS="$ENV_FILE_KEYS $key" ;;
        esac
    done < "$file"
}

# ─── 判断变量是否被显式指定（CLI 选项或环境变量文件） ────
is_explicitly_set() {
    local key="$1"
    # CLI 选项（CLI_CACHE_ENABLED 等）
    eval "cli_val=\${CLI_$key:-}"
    [ -n "$cli_val" ] && return 0
    # 环境变量文件
    case " $ENV_FILE_KEYS " in
        *" $key "*) return 0 ;;
    esac
    return 1
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
CLI_CACHE_HIT_DISCOUNT=""
CLI_CACHE_ENABLED=""
CLI_CACHE_TTL_SECS=""
CLI_CACHE_MAX_ENTRIES=""
CLI_CACHE_MAX_RESPONSE_BYTES=""
CLI_ENV_FILE=""
CLI_LOG_LINES=""

while [ $# -gt 0 ]; do
    case "$1" in
        --port)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} --port 需要指定端口号" >&2
                exit 1
            fi
            CLI_PORT="$2"
            INSTANCE_SCOPE="port"
            shift 2
            ;;
        --upstream-url)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} --upstream-url 需要指定 URL" >&2
                exit 1
            fi
            CLI_UPSTREAM="$2"
            shift 2
            ;;
        --timeout)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
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
        --cache-hit-discount)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} --cache-hit-discount 需要指定折扣比例" >&2
                exit 1
            fi
            CLI_CACHE_HIT_DISCOUNT="$2"
            shift 2
            ;;
        --cache-enabled)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} --cache-enabled 需要指定 true/false" >&2
                exit 1
            fi
            CLI_CACHE_ENABLED="$2"
            shift 2
            ;;
        --cache-ttl)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} --cache-ttl 需要指定秒数" >&2
                exit 1
            fi
            CLI_CACHE_TTL_SECS="$2"
            shift 2
            ;;
        --cache-max-entries)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} --cache-max-entries 需要指定条目数" >&2
                exit 1
            fi
            CLI_CACHE_MAX_ENTRIES="$2"
            shift 2
            ;;
        --cache-max-response-bytes)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} --cache-max-response-bytes 需要指定字节数" >&2
                exit 1
            fi
            CLI_CACHE_MAX_RESPONSE_BYTES="$2"
            shift 2
            ;;
        --lines)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} --lines 需要指定行数" >&2
                exit 1
            fi
            CLI_LOG_LINES="$2"
            shift 2
            ;;
        --all)
            INSTANCE_SCOPE="all"
            shift
            ;;
        -f|--env-file)
            if [ -z "${2:-}" ] || [ "${2#--}" != "${2:-}" ]; then
                printf '%b\n' "${RED}[错误]${NC} -f/--env-file 需要指定文件路径" >&2
                exit 1
            fi
            CLI_ENV_FILE="$2"
            shift 2
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

# ─── 加载环境变量文件（优先级: CLI 选项 > env文件 > 环境变量 > 默认值）───
ENV_FILE_KEYS=""   # 记录 env 文件中出现过的变量名（restart 时视为显式指定）
if [ -n "$CLI_ENV_FILE" ]; then
    load_env_file "$CLI_ENV_FILE"
fi

# ─── 合并配置优先级: CLI 选项 > 环境变量 > 默认值 ─────────
PORT="${CLI_PORT:-${PORT:-18318}}"
UPSTREAM_URL="${CLI_UPSTREAM:-${UPSTREAM_URL:-}}"
TIMEOUT="${CLI_TIMEOUT:-${TIMEOUT:-600}}"
DEBUG="${CLI_DEBUG:-${DEBUG:-false}}"
CACHE_HIT_DISCOUNT="${CLI_CACHE_HIT_DISCOUNT:-${CACHE_HIT_DISCOUNT:-}}"
CACHE_ENABLED="${CLI_CACHE_ENABLED:-${CACHE_ENABLED:-false}}"
CACHE_TTL_SECS="${CLI_CACHE_TTL_SECS:-${CACHE_TTL_SECS:-300}}"
CACHE_MAX_ENTRIES="${CLI_CACHE_MAX_ENTRIES:-${CACHE_MAX_ENTRIES:-100}}"
CACHE_MAX_RESPONSE_BYTES="${CLI_CACHE_MAX_RESPONSE_BYTES:-${CACHE_MAX_RESPONSE_BYTES:-102400}}"
LOG_LINES="${CLI_LOG_LINES:-${LOG_LINES:-50}}"   # 注意不用 LINES（bash 保留变量）

# ─── 从环境变量文件推断实例作用域 ────────────────────────
# 若使用了 -f env-file 且文件中包含 PORT（未显式传 --port/--all），
# 则自动将 stop/restart/status/log 的作用域锁定为该 PORT 对应实例，
# 无需再手动传 --port。
if [ -z "$INSTANCE_SCOPE" ] && [ -n "$CLI_ENV_FILE" ]; then
    case " $ENV_FILE_KEYS " in
        *" PORT "*)
            INSTANCE_SCOPE="port"
            ;;
    esac
fi

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
                saved_upstream=$(extract_upstream "$PORT")
                if [ "$saved_upstream" = "-" ]; then
                    printf '%b\n' "${RED}[错误]${NC} 无法获取实例 :$PORT 的上游地址，请显式指定 --upstream-url" >&2
                    exit 1
                fi
                UPSTREAM_URL="$saved_upstream"
            fi
            if [ -z "$CACHE_HIT_DISCOUNT" ]; then
                saved_discount=$(extract_cache_hit_discount "$PORT")
                if [ "$saved_discount" != "-" ]; then
                    CACHE_HIT_DISCOUNT="$saved_discount"
                fi
            fi
            # 清空未显式指定的缓存配置，restart 时从运行中的实例恢复
            # （CLI 选项或环境变量文件中出现的变量视为显式指定，优先于实例旧配置）
            if ! is_explicitly_set CACHE_ENABLED; then unset CACHE_ENABLED; fi
            if ! is_explicitly_set CACHE_TTL_SECS; then unset CACHE_TTL_SECS; fi
            if ! is_explicitly_set CACHE_MAX_ENTRIES; then unset CACHE_MAX_ENTRIES; fi
            if ! is_explicitly_set CACHE_MAX_RESPONSE_BYTES; then unset CACHE_MAX_RESPONSE_BYTES; fi
            restart_instance "$PORT" "$UPSTREAM_URL" "$CACHE_HIT_DISCOUNT"
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
        log_instance "$PORT" "$LOG_LINES"
        ;;
    list)
        list_instances
        ;;
    *)
        show_help
        exit 1
        ;;
esac
