# Mining Game Loop

The keeper is an automated cranker for the 1-minute mining game. It is not a privileged gatekeeper: `start_round` and `settle_round` remain permissionless on-chain instructions, so anyone can submit the same transactions if this service is down.

## Lifecycle

1. Read the `GameConfig` PDA at `["game_config"]`.
2. If `current_round_id == 0`, create round `1` with `start_round`.
3. Otherwise read `GameRound` at `["game_round", current_round_id]`.
4. While the round is open, log `end_slot`, `crank_slot`, and `player_count`.
5. After `end_slot + intermission_slots`, call `settle_round` with every `PlayerDeployment` PDA for that round.
6. If the round had fewer than 2 players, settlement marks it skipped and mints no tokens.
7. After settlement confirms, submit `start_round` for the next round.

## Accounts

`start_round` uses the keeper keypair as payer and derives:

- `GameConfig`: `["game_config"]`
- `GameRound`: `["game_round", next_round_id]`
- `RoundVault`: `["round_vault", next_round_id]`
- Previous `GameRound`: `["game_round", current_round_id]` after the first round

`settle_round` uses:

- `GameConfig`
- Current `GameRound`
- Reward mint stored in `GameConfig`
- `RoundVault`
- `SlotHashes` sysvar
- Token-2022 program
- All `PlayerDeployment` PDAs for the round

## Running

```bash
GAME_LOOP_INTERVAL_SECS=15 cargo run -- run-crons
```

`cargo run -- full` also runs the game loop alongside the HTTP API and other cron jobs.

## Manual Fallback

If the keeper is offline, submit the same `settle_round` instruction after `end_slot + intermission_slots`, then submit `start_round` for the next round. The account list must include every `PlayerDeployment` PDA for the round because the current MVP settlement writes all results in one transaction.
