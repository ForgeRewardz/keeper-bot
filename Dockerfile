# syntax=docker/dockerfile:1.6
#
# =============================================================================
# mvp-keeper-bot — multi-stage build with cargo-chef + BuildKit cache mounts
# =============================================================================
# Builds the REWARDZ keeper-bot Rust binary using the cargo-chef pattern for
# fast incremental rebuilds on CI/Railway:
#
#   Stage 1 (planner): cargo chef prepare → recipe.json (layered dep manifest)
#   Stage 2 (builder): cargo chef cook → prebuilt deps cached by BuildKit
#   Stage 3 (builder): final binary build (uses cached deps)
#   Stage 4 (runtime): minimal alpine + entrypoint for KEYPAIR_BASE64 decode
#
# The `--mount=type=cache` directives preserve cargo's registry/git/target
# across builds so a cold Railway rebuild only recompiles changed code,
# not the full Solana dep tree. Trims typical rebuild from ~15min to ~2min.
#
# Build locally with: DOCKER_BUILDKIT=1 docker build -t rewardz-keeper .
# =============================================================================

FROM rust:1.82-alpine AS chef
RUN apk add --no-cache musl-dev pkgconfig openssl-dev bash
RUN cargo install cargo-chef --locked --version 0.1.68
WORKDIR /app

# ---- planner: capture the dep graph only ----
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder: cook deps (cached), then build the actual binary ----
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cook: build only dependencies, not the workspace crates.
# BuildKit cache mounts persist across builds on the same runner.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY src/ src/

# Final build. Cache mounts give us: registry + git + target dir.
# After build, copy the binary OUT of the cached target dir so it
# survives into the next stage (cache mounts aren't preserved in layers).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release && \
    cp target/release/mvp-keeper-bot /mvp-keeper-bot

# ---- runtime: minimal alpine + entrypoint for KEYPAIR_BASE64 decode ----
FROM alpine:3.20 AS runtime
RUN apk add --no-cache ca-certificates bash
COPY scripts/entrypoint.sh /entrypoint.sh
COPY --from=builder /mvp-keeper-bot /usr/local/bin/
RUN chmod +x /entrypoint.sh

EXPOSE 8081
ENTRYPOINT ["/entrypoint.sh"]
CMD ["full"]
