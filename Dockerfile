# ============================================================
# 多阶段构建：编译 + 运行
# ============================================================

# 阶段1：编译（需 Rust >= 1.85 以支持 edition2024 依赖）
FROM rust:1.97-slim AS builder

WORKDIR /app
COPY . .

RUN cargo build --release

# 阶段2：极小运行镜像
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/stream-converter /usr/local/bin/stream-converter

ENV UPSTREAM_URL=http://127.0.0.1:8317
ENV TIMEOUT=600
ENV DEBUG=false
ENV PORT=18318

EXPOSE 18318

CMD ["stream-converter"]
