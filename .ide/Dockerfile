# ============================================================
# 自定义开发环境镜像
#
# 以 CNB 默认开发环境为基础（已内置 code-server / VSCode Web
# 界面与常用插件），在此之上补齐 Rust 完整工具链与编译依赖，
# 这样下次启动云原生开发时既有 VSCode 界面，也能直接
# `cargo build --release` 快速编译启动。
# ============================================================

# 默认开发环境镜像已预装 cnbcool.cnb-welcome 插件与 code-server，
# 基于该镜像扩展可保留 VSCode Web 开发界面。
FROM cnbcool/default-dev-env:latest AS dev

# ─── 安装编译所需系统依赖 ──────────────────────────────────
#   - build-essential : 提供 gcc/cc 链接器与 make（Rust 编译必需）
#   - pkg-config / libssl-dev : reqwest 的 TLS 依赖编译需要
#   - ca-certificates : 访问上游 HTTPS 需要
#   - curl / jq       : 常用开发与调试工具
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        ca-certificates \
        curl \
        jq \
    && rm -rf /var/lib/apt/lists/*

# ─── 安装 Rust 工具链（rustup 方式，稳定版）─────────────────
# 若基础镜像已带 rustc/cargo，此步骤会自动复用，不会重复安装。
RUN if ! command -v rustc >/dev/null 2>&1; then \
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --default-toolchain stable \
    ; fi
ENV PATH="/root/.cargo/bin:${PATH}"

# ─── 安装 VSCode 简体中文语言包并设为默认界面语言 ────────────
# MS-CEINTL.vscode-language-pack-zh-hans 为微软官方简体中文包，
# 配合 locale.json 配置使 code-server 启动时直接使用中文界面。
RUN code-server --install-extension MS-CEINTL.vscode-language-pack-zh-hans \
    && mkdir -p /root/.local/share/code-server/User \
    && echo '{"locale":"zh-cn"}' > /root/.local/share/code-server/User/locale.json

# ─── 指定字符集，支持命令行中文输入 ──────────────────────────
ENV LANG=C.UTF-8
ENV LANGUAGE=C.UTF-8

# ─── 验证环境就绪（cargo + 链接器 cc 均需可用）─────────────
RUN rustc --version && cargo --version && cc --version | head -1

# 开发环境入口
WORKDIR /workspace
CMD ["/bin/bash"]
