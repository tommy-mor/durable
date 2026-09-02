FROM rust:bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      clang libclang-dev cmake pkg-config libssl-dev curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
RUN rustup target add wasm32-unknown-unknown

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY durable ./durable
COPY durable-derive ./durable-derive
COPY hoprt ./hoprt
COPY hop-web ./hop-web
COPY rank ./rank

RUN cargo build --release -p hoprt --bin hopd --features server
RUN wasm-pack build hop-web --target web --release --out-dir /src/hop-web/pkg

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates python3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=caddy:2-alpine /usr/bin/caddy /usr/bin/caddy
COPY --from=builder /src/target/release/hopd /usr/local/bin/hopd
COPY --from=builder /src/hop-web/pkg /app/hop-web/pkg
COPY hoprt/scripts/port_ember_log.py /app/port_ember_log.py
COPY deploy/ember/ember.hop /app/ember.hop
COPY deploy/ember/Caddyfile /app/Caddyfile
COPY deploy/ember/start.sh /app/start.sh
RUN chmod +x /app/start.sh
ENV HOP_WS_PATH=/ws
EXPOSE 3000
CMD ["/app/start.sh"]
