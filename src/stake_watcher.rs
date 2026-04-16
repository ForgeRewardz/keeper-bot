// ============================================================
// stake_watcher.rs — on-chain ProtocolStake mirror + capacity unlock.
//
// Plan task 16a (Colosseum Rewardz League). Periodically polls each
// league-participating protocol's `ProtocolStake` PDA, mirrors
// (direct_stake + rented_stake) into `protocols.active_stake`, and on
// the FIRST-EVER transition from unstaked to any non-zero stake
// (gated by the sticky `capacity_unlocked_at` column from migration
// 046) resets `remaining_capacity` to `issuance_ratio × active_stake`
// per league-config.md §Capacity ("stake to unlock capacity").
// Subsequent stake changes (including a later unstake→re-stake cycle)
// only update `active_stake`; the capacity_reset cron
// (capacity_reset.rs) owns any window refresh. This prevents a
// protocol from cycling stake to refresh its capacity window
// mid-cadence — a capacity-farming vector if the unlock branch
// fired on every 0→non-zero transition.
//
// This is a polling watcher rather than a websocket subscription to
// keep the operational surface minimal (same RPC client pattern as
// publisher.rs / game_loop.rs — no long-lived streams to reconnect).
// Cadence default: 60s. Devnet rehearsals can shorten via env.
//
// Emits a `protocol_events` row (`kind='capacity_unlocked'`, level='info')
// on the unlock transition so the console can surface a banner.
//
// # Operational notes
//
// - **LeagueConfig is captured at cron spawn and treated as
//   process-lifetime immutable.** Changing env/config values requires
//   a keeper restart to take effect. This matches the rest of the
//   keeper's crons and keeps sample→sample semantics consistent
//   within a single run.
//
// - **Per-tick cost is O(N) sequential RPC calls**, where N is the
//   count of league-joined protocols. At 60s cadence + 150ms devnet
//   RPC latency, sequential polling is comfortable up to a few
//   hundred protocols; beyond that, wrap the per-protocol loop in a
//   bounded-concurrency `FuturesUnordered`. `MissedTickBehavior::Delay`
//   prevents stacking if a tick exceeds the interval, but the
//   per-tick upper bound is still linear in N.
// ============================================================

use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use sqlx::postgres::PgPool;
use std::str::FromStr;
use std::time::Duration;
use tokio::time::{self, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use crate::league_config::{load_league_config, LeagueConfig};

/// PDA seed for ProtocolStake (matches mvp-smart-contracts consts).
const PROTOCOL_STAKE_SEED: &[u8] = b"protocol_stake";

/// Byte layout mirrors `api/src/state/protocol_stake.rs`:
///   - data[0]      discriminator
///   - data[1]      account version
///   - data[2..34]  authority (32B)
///   - data[34..42] direct_stake (u64 LE)
///   - data[42..50] rented_stake (u64 LE)
///
/// Exposed as constants so a future ABI change (e.g. a struct bump)
/// fails loudly at the parse layer rather than silently reading the
/// wrong field.
const OFF_DIRECT_STAKE: usize = 2 + 32;
const OFF_RENTED_STAKE: usize = 2 + 40;
const MIN_ACCOUNT_LEN: usize = OFF_RENTED_STAKE + 8;

/// One protocol's row as read from DB. Kept minimal: the watcher only
/// needs the authority pubkey (for PDA derivation), the mirrored
/// stake (to detect value changes and skip no-op UPDATEs), and the
/// sticky `capacity_unlocked_at` marker (to gate the unlock branch).
#[derive(Debug)]
struct ProtocolRow {
    id: sqlx::types::Uuid,
    admin_wallet: String,
    active_stake: Option<i64>,
    already_unlocked: bool,
}

/// Fetch every league-joined protocol. A protocol "is in the league"
/// when it has a referral_code assigned (mirrors the /league/join
/// handler in api/src/routes/protocols.ts — that handler is the only
/// writer that assigns a referral_code). Filtering here avoids
/// polling PDAs for protocols that have not opted in, keeping RPC
/// load proportional to league membership instead of total protocols.
async fn load_protocols(pool: &PgPool) -> sqlx::Result<Vec<ProtocolRow>> {
    let rows = sqlx::query_as::<_, (sqlx::types::Uuid, String, Option<i64>, Option<chrono::DateTime<chrono::Utc>>)>(
        r#"
        SELECT id, admin_wallet, active_stake, capacity_unlocked_at
          FROM protocols
         WHERE referral_code IS NOT NULL
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, admin_wallet, active_stake, capacity_unlocked_at)| ProtocolRow {
            id,
            admin_wallet,
            active_stake,
            already_unlocked: capacity_unlocked_at.is_some(),
        })
        .collect())
}

/// Read (direct_stake + rented_stake) from the on-chain ProtocolStake
/// account. Returns `Ok(None)` when the PDA does not exist yet — a
/// normal state for a protocol that has joined the league but has
/// not yet staked. Returns `Err` only on RPC / parse errors.
///
/// Uses `get_account_with_commitment` so a missing account comes back
/// as `Ok(Response { value: None })` rather than an RPC error — lets
/// the caller branch cleanly on Some/None instead of string-matching
/// Debug output of `ClientError`, which is fragile across solana-client
/// version bumps.
fn read_active_stake(
    rpc: &RpcClient,
    program_id: &Pubkey,
    authority: &Pubkey,
) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
    let (pda, _) = Pubkey::find_program_address(
        &[PROTOCOL_STAKE_SEED, authority.as_ref()],
        program_id,
    );

    let response = rpc.get_account_with_commitment(&pda, CommitmentConfig::confirmed())?;
    let account = match response.value {
        Some(a) => a,
        None => return Ok(None),
    };

    if account.data.len() < MIN_ACCOUNT_LEN {
        return Err(format!(
            "ProtocolStake account {pda} too small: {} < {MIN_ACCOUNT_LEN}",
            account.data.len()
        )
        .into());
    }

    let direct =
        u64::from_le_bytes(account.data[OFF_DIRECT_STAKE..OFF_DIRECT_STAKE + 8].try_into()?);
    let rented =
        u64::from_le_bytes(account.data[OFF_RENTED_STAKE..OFF_RENTED_STAKE + 8].try_into()?);

    // Saturating add: an overflow here would be nonsensical economic
    // state, but panicking would take the whole watcher down. Clamp
    // to u64::MAX and let the anti-abuse cron flag it.
    Ok(Some(direct.saturating_add(rented)))
}

/// Apply one observed stake sample for a protocol. Returns true if a
/// `capacity_unlocked` event was emitted (first-ever unlock transition).
///
/// Gating rule: unlock fires on the first observation of a non-zero
/// stake where `capacity_unlocked_at IS NULL` (sticky marker from
/// migration 046, surfaced as `already_unlocked`). Subsequent stake
/// changes — including a later unstake→re-stake cycle — only mirror
/// `active_stake`; any capacity window refresh is owned by
/// `capacity_reset.rs`. See module header for the capacity-farming
/// rationale.
async fn apply_sample(
    pool: &PgPool,
    cfg: &LeagueConfig,
    protocol_id: sqlx::types::Uuid,
    observed_stake: u64,
    prev_stake: Option<i64>,
    already_unlocked: bool,
) -> sqlx::Result<bool> {
    // Cast to i64 for the DB column type; saturating to i64::MAX is
    // safe because u64 values above that are nonsensical stake amounts
    // (would exceed the token's max supply by many OOMs).
    let observed_i64 = observed_stake.min(i64::MAX as u64) as i64;
    let is_first_unlock = observed_stake > 0 && !already_unlocked;

    // Fast-path: if the mirrored value is unchanged and we're not
    // transitioning to unlocked, skip the whole transaction. Avoids N
    // no-op WAL writes per tick across all steady-state protocols.
    if !is_first_unlock && prev_stake == Some(observed_i64) {
        return Ok(false);
    }

    let mut tx = pool.begin().await?;

    // Mirror active_stake so downstream capacity_baseline() lookups
    // in api/src/services/capacity.ts see the current value. Guarded
    // by IS DISTINCT FROM so a re-entrant tick after a partial earlier
    // failure doesn't rewrite the same value.
    sqlx::query(
        r#"
        UPDATE protocols
           SET active_stake = $2
         WHERE id = $1
           AND active_stake IS DISTINCT FROM $2
        "#,
    )
    .bind(protocol_id)
    .bind(observed_i64)
    .execute(&mut *tx)
    .await?;

    if is_first_unlock {
        // Reset remaining_capacity to the new baseline. This is the
        // "stake unlocks capacity" semantic: the starter grant
        // (small) is superseded by the stake-derived ceiling (larger
        // for any non-trivial stake given issuance_ratio >= 10 on
        // devnet). We overwrite rather than additively top-up because
        // the window semantics (capacity_window_start) re-anchor to
        // NOW() — the unlock is a fresh window, not a refill.
        let new_capacity = (cfg.issuance_ratio as u128) * (observed_stake as u128);
        let new_capacity_i64 = new_capacity.min(i64::MAX as u128) as i64;
        // Sticky `capacity_unlocked_at = NOW()` is set in the same
        // statement as the window/remaining reset so the gate flips
        // atomically with the capacity effects. A later tick observing
        // `capacity_unlocked_at IS NOT NULL` will short-circuit the
        // unlock branch — preventing unstake→re-stake farming.
        sqlx::query(
            r#"
            UPDATE protocols
               SET remaining_capacity = $2,
                   capacity_window_start = NOW(),
                   capacity_unlocked_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(protocol_id)
        .bind(new_capacity_i64)
        .execute(&mut *tx)
        .await?;

        // Emit the transition as a protocol_events row so the console
        // can surface a banner. Payload includes the baseline and
        // stake in text form (same convention as capacity_warning
        // events — bigint values carried as strings avoid JSON-number
        // precision loss on mobile clients).
        sqlx::query(
            r#"
            INSERT INTO protocol_events (protocol_id, kind, level, payload)
            VALUES ($1, 'capacity_unlocked', 'info', $2::jsonb)
            "#,
        )
        .bind(protocol_id)
        .bind(serde_json::json!({
            "active_stake": observed_stake.to_string(),
            "issuance_ratio": cfg.issuance_ratio,
            "new_capacity": new_capacity_i64.to_string(),
        }))
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(is_first_unlock)
}

async fn tick(
    pool: &PgPool,
    rpc: &RpcClient,
    cfg: &LeagueConfig,
    program_id: &Pubkey,
) -> Result<(u64, u64), Box<dyn std::error::Error + Send + Sync>> {
    let protocols = load_protocols(pool).await?;
    let mut observed = 0u64;
    let mut unlocked = 0u64;

    for proto in protocols {
        let authority = match Pubkey::from_str(&proto.admin_wallet) {
            Ok(pk) => pk,
            Err(e) => {
                warn!(
                    "[stake-watcher] skipping protocol {id}: invalid admin_wallet {wallet}: {e}",
                    id = proto.id,
                    wallet = proto.admin_wallet,
                );
                continue;
            }
        };

        let stake = match read_active_stake(rpc, program_id, &authority) {
            Ok(Some(s)) => s,
            Ok(None) => {
                debug!(
                    "[stake-watcher] no ProtocolStake PDA for {wallet} yet",
                    wallet = proto.admin_wallet
                );
                continue;
            }
            Err(e) => {
                warn!(
                    "[stake-watcher] RPC error for protocol {id}: {e}",
                    id = proto.id
                );
                continue;
            }
        };

        observed += 1;
        match apply_sample(
            pool,
            cfg,
            proto.id,
            stake,
            proto.active_stake,
            proto.already_unlocked,
        )
        .await
        {
            Ok(true) => {
                unlocked += 1;
                info!(
                    "[stake-watcher] capacity unlocked for protocol {id}: stake={stake}",
                    id = proto.id
                );
            }
            Ok(false) => {}
            Err(e) => {
                warn!(
                    "[stake-watcher] DB error applying sample for {id}: {e}",
                    id = proto.id
                );
            }
        }
    }

    Ok((observed, unlocked))
}

/// Spawn the stake-watcher cron. `interval_secs` controls the RPC
/// poll cadence; each tick iterates every league-joined protocol.
/// For devnet rehearsals, set STAKE_WATCHER_INTERVAL_SECS=15 so the
/// unlock banner shows up within the test window.
pub fn start_stake_watcher_cron(interval_secs: u64, pool: PgPool, rpc_url: String, program_id: Pubkey) {
    let cfg = load_league_config();

    tokio::spawn(async move {
        let rpc = RpcClient::new(rpc_url);
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        // Skip missed ticks rather than burst-catch-up — under RPC
        // backpressure we'd rather drop samples than stack requests.
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Skip the initial immediate tick so other crons can warm up
        // before we start hitting the RPC endpoint.
        interval.tick().await;

        loop {
            interval.tick().await;
            match tick(&pool, &rpc, &cfg, &program_id).await {
                Ok((0, _)) => { /* silent no-op tick */ }
                Ok((observed, 0)) => {
                    debug!("[stake-watcher] observed {observed} protocol(s), no unlocks");
                }
                Ok((observed, unlocked)) => {
                    info!("[stake-watcher] observed {observed}, unlocked {unlocked}");
                }
                Err(e) => error!("[stake-watcher] tick error: {e}"),
            }
        }
    });
}
