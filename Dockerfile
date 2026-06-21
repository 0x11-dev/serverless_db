FROM docker.1panel.live/library/rust:1.83.0-bookworm AS build

WORKDIR /app
RUN mkdir -p /usr/local/cargo \
    && printf '%s\n' \
      '[source.crates-io]' \
      'replace-with = "rsproxy-sparse"' \
      '' \
      '[source.rsproxy-sparse]' \
      'registry = "sparse+https://rsproxy.cn/index/"' \
      '' \
      '[net]' \
      'git-fetch-with-cli = true' \
      > /usr/local/cargo/config.toml
COPY rust-core/Cargo.toml rust-core/Cargo.lock ./rust-core/
COPY rust-core/src ./rust-core/src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/rust-core/target \
    cargo build --release --manifest-path rust-core/Cargo.toml --features s3 --bin serverless-db-core \
    && cp rust-core/target/release/serverless-db-core /tmp/serverless-db-core

FROM docker.1panel.live/library/debian:bookworm-20241202-slim

RUN sed -i 's|http://deb.debian.org/debian|http://mirrors.aliyun.com/debian|g; s|http://deb.debian.org/debian-security|http://mirrors.aliyun.com/debian-security|g' /etc/apt/sources.list /etc/apt/sources.list.d/*.sources 2>/dev/null || true \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /tmp/serverless-db-core /usr/local/bin/serverless-db-core

EXPOSE 8765
ENTRYPOINT ["serverless-db-core"]
