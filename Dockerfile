FROM rust:1.89-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release; rm -rf src
COPY src ./src
# COPY can preserve source mtimes older than the cached dummy-build
# artifacts, in which case cargo sees a fresh fingerprint, skips the
# rebuild, and we'd ship the dummy binary. Touch forces the real build.
RUN find src -type f -exec touch -c {} +
RUN cargo build --release --locked && cp target/release/fake-sms-server /fake-sms-server
# Guardrail: the real binary links tokio/axum (many MB); the dummy
# main is ~300KB. Fail the build rather than ship a no-op server.
RUN test $(stat -c%s /fake-sms-server) -gt 1000000

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /fake-sms-server /usr/local/bin/fake-sms-server
ENV HOST=0.0.0.0 PORT=8080
EXPOSE 8080
CMD ["fake-sms-server"]
