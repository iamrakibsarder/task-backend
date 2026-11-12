# TaskX API. Build: docker build -t taskx-api taskx-backend
FROM rust:1-slim-bookworm AS build
WORKDIR /app
# Cache dependencies separately from the app code.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs \
    && cargo build --release --locked 2>/dev/null || true
COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 taskx && mkdir -p /data/screenshots && chown taskx /data/screenshots
COPY --from=build /app/target/release/taskx-backend /usr/local/bin/taskx-backend
USER taskx
ENV HOST=0.0.0.0 PORT=8080 SCREENSHOT_DIR=/data/screenshots RUST_LOG=taskx_backend=info,tower_http=info
EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s CMD curl -sf http://127.0.0.1:8080/health || exit 1
CMD ["taskx-backend"]
