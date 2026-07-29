# ProxyGit Server — multi-stage Rust build
#
# Environment variables (all optional):
#   PROXYGIT_LISTEN      — listen address, default "0.0.0.0:8080"
#   PROXYGIT_DATA_DIR    — data directory, default "/data"
#
# Ports:
#   8080/udp — QUIC transport (client↔server protocol)
#   3900    — WebDAV HTTP (native macOS Finder mount, no kernel extension needed)

FROM rust:1.96-slim-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN cargo build --release --bin proxygit-server

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/proxygit-server /usr/local/bin/proxygit-server

EXPOSE 8080/udp

ENV PROXYGIT_DATA_DIR=/data

VOLUME ["/data"]

ENTRYPOINT ["proxygit-server"]
