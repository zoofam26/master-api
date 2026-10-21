# ---- build stage ------------------------------------------------------------
# Latest stable Rust: crates.io dependencies in Cargo.lock (e.g. crypto-common
# 0.2) use edition2024, which needs cargo >= 1.85.
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# Cache dependencies in a separate layer: a manifest change is the only thing
# that invalidates them.
COPY Cargo.toml Cargo.lock ./
COPY crates/common/Cargo.toml crates/common/Cargo.toml
COPY crates/youtube/Cargo.toml crates/youtube/Cargo.toml
COPY crates/books/Cargo.toml crates/books/Cargo.toml
COPY crates/countries/Cargo.toml crates/countries/Cargo.toml
COPY crates/currency/Cargo.toml crates/currency/Cargo.toml
COPY crates/earthquake/Cargo.toml crates/earthquake/Cargo.toml
COPY crates/ip_geolocation/Cargo.toml crates/ip_geolocation/Cargo.toml
COPY crates/postal_code/Cargo.toml crates/postal_code/Cargo.toml
COPY crates/public_holidays/Cargo.toml crates/public_holidays/Cargo.toml
COPY crates/transit/Cargo.toml crates/transit/Cargo.toml
COPY crates/university/Cargo.toml crates/university/Cargo.toml
COPY crates/weather/Cargo.toml crates/weather/Cargo.toml
COPY crates/app/Cargo.toml crates/app/Cargo.toml

# Stub every source file so `cargo build` can materialise the dependency
# graph; the real sources are copied over them afterwards.
RUN for c in common youtube books countries currency earthquake ip_geolocation \
             postal_code public_holidays transit university weather app; do \
      mkdir -p "crates/$c/src" && echo "" > "crates/$c/src/lib.rs"; \
    done && \
    echo 'fn main() {}' > crates/app/src/main.rs && \
    cargo build --release --workspace && \
    rm -rf crates/*/src crates/*/tests

# Copy the real sources and build the actual workspace. Touch every source so
# cargo does not reuse the stub artefacts.
COPY crates crates
RUN find crates -name "*.rs" -exec touch {} + && \
    cargo build --release --workspace && \
    strip target/release/master-api

# ---- runtime stage ----------------------------------------------------------
# The binary is statically self-contained (rustls TLS, no system OpenSSL), so
# the runtime image only needs CA certificates and a tzdata copy.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged user.
RUN useradd --system --uid 10001 --shell /usr/sbin/nologin masterapi
USER masterapi

WORKDIR /app
COPY --from=builder /build/target/release/master-api /app/master-api

ENV APP_HOST=0.0.0.0 \
    APP_PORT=8080 \
    RUST_LOG=info

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -fsS http://localhost:8080/health || exit 1

ENTRYPOINT ["/app/master-api"]
