# syntax=docker/dockerfile:1.6
#
# =============================================================================
# mvp-keeper-bot — multi-stage build with cargo-chef + BuildKit cache mounts
# =============================================================================
# Build context: mobileSpecs/ root (set in docker-compose.yml).
# Reason: mvp-keeper-bot/Cargo.toml has a path dep on the on-chain API crate:
#     rewardz-mvp-api = { path = "../mvp-smart-contracts/api" }
# A per-service context can't see ../mvp-smart-contracts, so cargo errors with
# `error: failed to load manifest for dependency`. Lifting the context to the
# monorepo root lets us COPY the sibling crate at the expected relative path.
#
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
# =============================================================================

FROM rust:1.89-alpine AS chef
# openssl-libs-static is mandatory for musl static linking — the openssl-sys
# crate (transitively pulled in by reqwest/native-tls) needs both the headers
# (openssl-dev) and the static archives (openssl-libs-static), otherwise the
# final link fails with `cannot find -lssl / -lcrypto` against alpine musl.
RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static bash
RUN cargo install cargo-chef --locked --version 0.1.68
WORKDIR /app/mvp-keeper-bot

# ---- planner: capture the dep graph only ----
# Layout the workspace such that ../mvp-smart-contracts/api resolves
# correctly (matches host layout exactly).
FROM chef AS planner
COPY mvp-smart-contracts/Cargo.toml mvp-smart-contracts/Cargo.lock /app/mvp-smart-contracts/
COPY mvp-smart-contracts/api/ /app/mvp-smart-contracts/api/
COPY mvp-smart-contracts/program/ /app/mvp-smart-contracts/program/
COPY mvp-smart-contracts/cli/ /app/mvp-smart-contracts/cli/
COPY mvp-keeper-bot/Cargo.toml mvp-keeper-bot/Cargo.lock ./
COPY mvp-keeper-bot/src/ src/
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder: cook deps (cached), then build the actual binary ----
FROM chef AS builder
# Sibling crate must be present BEFORE cook because the recipe references it.
COPY mvp-smart-contracts/Cargo.toml mvp-smart-contracts/Cargo.lock /app/mvp-smart-contracts/
COPY mvp-smart-contracts/api/ /app/mvp-smart-contracts/api/
COPY mvp-smart-contracts/program/ /app/mvp-smart-contracts/program/
COPY mvp-smart-contracts/cli/ /app/mvp-smart-contracts/cli/
COPY --from=planner /app/mvp-keeper-bot/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/mvp-keeper-bot/target \
    cargo chef cook --release --recipe-path recipe.json

COPY mvp-keeper-bot/Cargo.toml mvp-keeper-bot/Cargo.lock ./
COPY mvp-keeper-bot/src/ src/

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/mvp-keeper-bot/target \
    cargo build --release && \
    cp target/release/mvp-keeper-bot /mvp-keeper-bot

# ---- runtime: minimal alpine + entrypoint for KEYPAIR_BASE64 decode ----
FROM alpine:3.20 AS runtime
RUN apk add --no-cache ca-certificates bash
COPY mvp-keeper-bot/scripts/entrypoint.sh /entrypoint.sh
COPY --from=builder /mvp-keeper-bot /usr/local/bin/
RUN chmod +x /entrypoint.sh

EXPOSE 8081
ENTRYPOINT ["/entrypoint.sh"]
CMD ["full"]
