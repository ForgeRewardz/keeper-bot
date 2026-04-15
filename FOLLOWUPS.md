# Follow-ups

## F6 — Keeper-bot constants extraction (2026-04-15)

### Deploy-path: Dockerfile build context does not include `rewardz-mvp-api` path dep

F6 added `rewardz-mvp-api = { path = "../mvp-smart-contracts/api" }` to `Cargo.toml` to replace hardcoded discriminators / PDA seeds / program IDs. This works for local `cargo check` but breaks the Docker build because the Dockerfile only copies `Cargo.toml Cargo.lock src/` — the sibling crate is outside the docker build context.

**Options:**

- Move keeper-bot + mvp-smart-contracts under a shared Cargo workspace with a root-level Dockerfile that copies both trees
- Publish `rewardz-mvp-api` to a registry or pin a git dep with a rev
- Add a repo-root Dockerfile that `COPY`s both subtrees and adjusts `WORKDIR`

Not urgent — Docker deploy is not on the critical path this session, and `cargo check` / local runs work. Track for batch 6 (F12) cross-repo verification.

### Nice-to-haves (from F6 code review)

- Add `pub const PREFIX_LEN: usize = 2` to `rewardz-mvp-api::consts` so keeper-bot can drop its local mirror
- Add `parse_u32` helper to `rewardz-mvp-api` alongside existing `parse_u16` / `parse_u64` / `parse_i64` / `parse_u128` / `parse_pubkey`
- Declare `TOKEN_2022_PROGRAM_ID` as a `const Pubkey` (via `solana_sdk::pubkey!` macro or `spl_token_2022::ID`) instead of `from_str(...).unwrap()` at each call site
- Document `KEEPER_RUN_CRANKER` env var in README Environment Variables table
