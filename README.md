# mvp-keeper-bot

Off-chain keeper service for the REWARDZ protocol. Publishes Merkle roots on-chain, signs point-sync receipts, settles rentals, executes subscriptions, and cranks the mining game loop.

## Architecture

The keeper bot runs as a single binary with three modes:

- **serve-api** — HTTP server (receipt signing, bootstrap awards, health check)
- **run-crons** — Background jobs (root publisher, rental settlement, subscription executor, mining game loop)
- **full** — Both API and crons (default)

### Components

| Module             | Purpose                                           |
| ------------------ | ------------------------------------------------- |
| `merkle.rs`        | Merkle tree library (Keccak256, matches on-chain) |
| `publisher.rs`     | Publishes Merkle roots on-chain every 8h          |
| `api.rs`           | Receipt signing + bootstrap award HTTP endpoints  |
| `rental.rs`        | Settles active rental agreements weekly           |
| `subscriptions.rs` | Polls and executes due subscriptions              |
| `game_loop.rs`     | Starts and settles mining rounds                  |

## Environment Variables

| Variable                          | Required | Default                 | Description                  |
| --------------------------------- | -------- | ----------------------- | ---------------------------- |
| `DATABASE_URL`                    | Yes      | —                       | PostgreSQL connection string |
| `KEYPAIR`                         | Yes      | —                       | Path to admin keypair JSON   |
| `PROGRAM_ID`                      | Yes      | —                       | Deployed program address     |
| `INTERNAL_API_KEY`                | Yes      | —                       | Key for internal API calls   |
| `SOLANA_RPC_URL`                  | No       | `http://127.0.0.1:8899` | Solana RPC endpoint          |
| `API_LISTEN_ADDR`                 | No       | `0.0.0.0:8081`          | HTTP listen address          |
| `API_BASE_URL`                    | No       | `http://127.0.0.1:3001` | API service URL              |
| `POINT_ROOT_INTERVAL_SECS`        | No       | `28800` (8h)            | Root publish interval        |
| `RENTAL_SETTLE_INTERVAL_SECS`     | No       | `604800` (1 week)       | Rental settlement interval   |
| `SUBSCRIPTION_POLL_INTERVAL_SECS` | No       | `60`                    | Subscription poll interval   |
| `GAME_LOOP_INTERVAL_SECS`         | No       | `15`                    | Mining game crank interval   |
| `POINTS_REQUEST_TTL_SECONDS`      | No       | `300`                   | Receipt request TTL          |
| `POINTS_RECEIPT_TTL_SECONDS`      | No       | `600`                   | Signed receipt TTL           |
| `POINTS_CORS_ORIGINS`             | No       | `*`                     | CORS allowed origins         |

## Running

### Cargo (development)

```bash
cargo run -- full
cargo run -- serve-api
cargo run -- run-crons
```

### Docker Compose

```bash
# Set required env vars
export PROGRAM_ID=YourProgramId...
export INTERNAL_API_KEY=your-secret-key

# Place admin keypair
mkdir -p keys && cp ~/.config/solana/id.json keys/admin.json

# Start
docker compose up -d
```

## API Endpoints

| Method | Path               | Description                       |
| ------ | ------------------ | --------------------------------- |
| POST   | `/sign-receipt`    | Sign a point-sync receipt         |
| POST   | `/bootstrap-award` | Award initial points to new users |
| GET    | `/healthz`         | Health check                      |

## Mining Game Loop

The keeper polls the game PDAs and submits permissionless round transactions:

- `start_round` when no round exists or the previous round is settled and past intermission
- `settle_round` after `end_slot + intermission_slots`
- skipped settlement when a round has fewer than 2 players

See [`GAME_LOOP.md`](./GAME_LOOP.md) for account lists, timing, and manual fallback notes.

## Merkle Tree

The Merkle tree uses Keccak256 (sha3 crate) with sorted pair hashing to match the on-chain verification in `mvp-smart-contracts/program/src/points.rs`:

- Leaf: `keccak256(authority_bytes || points_le_bytes)`
- Pair: `keccak256(min(left, right) || max(left, right))`
