// ============================================================
// capacity_reset.rs — Periodic remaining_capacity reset cron.
//
// Resets `protocols.remaining_capacity` to the league's `starter_grant_rewardz`
// baseline whenever a protocol's capacity_window has elapsed (cadence per
// `LeagueConfig.capacity_reset_cadence`). Also bootstraps protocols whose
// capacity_window_start is NULL (newly-joined league participants).
//
// Devnet override: setting `LEAGUE_CAPACITY_RESET_OVERRIDE_SECS=300` shortens
// the cadence to 5 minutes for end-to-end testing without touching code.
// ============================================================

use sqlx::postgres::PgPool;
use std::time::Duration;
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info, warn};

use crate::league_config::{load_league_config, LeagueConfig, ResetCadence};

/// Map cadence enum → seconds. Yearly is treated as effectively-never for
/// mainnet zero-placeholder mode (the cron still runs but nothing matches).
fn cadence_secs(cadence: ResetCadence) -> u64 {
    match cadence {
        ResetCadence::Weekly => 7 * 86_400,
        ResetCadence::Yearly => 365 * 86_400,
    }
}

fn override_secs() -> Option<u64> {
    // Unset → None (normal path). Set-but-unparseable → panic. A silent
    // fall-through to the real cadence would be a devnet footgun: "I set
    // the override, why isn't it firing?"
    std::env::var("LEAGUE_CAPACITY_RESET_OVERRIDE_SECS").ok().map(|s| {
        s.parse::<u64>()
            .unwrap_or_else(|e| panic!("LEAGUE_CAPACITY_RESET_OVERRIDE_SECS={s:?} must be u64: {e}"))
    })
}

/// Reset remaining_capacity for any protocol whose window has elapsed
/// (or whose window has never been initialised). Returns the number of
/// rows affected so the caller can log/observe.
async fn reset_capacities(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<u64> {
    let cadence = override_secs().unwrap_or_else(|| cadence_secs(cfg.capacity_reset_cadence));
    // i64 cast is safe: cadence is at most 365*86400 < i32::MAX.
    let cadence_i64 = cadence as i64;
    let baseline = cfg.starter_grant_rewardz as i64;

    // Single-statement reset. The WHERE clause covers two cases:
    //   1. capacity_window_start IS NULL — never bootstrapped, give the
    //      protocol its starter grant.
    //   2. NOW() - capacity_window_start >= cadence — window elapsed.
    // remaining_capacity is set unconditionally to the baseline; we do NOT
    // top-up partial usage (per league-config.md §Capacity reset).
    let result = sqlx::query(
        r#"
        UPDATE protocols
           SET remaining_capacity = $1,
               capacity_window_start = NOW()
         WHERE capacity_window_start IS NULL
            OR NOW() - capacity_window_start >= make_interval(secs => $2)
        "#,
    )
    .bind(baseline)
    .bind(cadence_i64)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

/// Spawn the cron loop. `interval_secs` controls how often we check the
/// elapsed-window predicate; the cadence itself is per-protocol and lives
/// in `capacity_window_start`. Default poll interval should be << cadence.
pub fn start_capacity_reset_cron(interval_secs: u64, pool: PgPool) {
    let cfg = load_league_config();
    if let Some(s) = override_secs() {
        warn!(
            "[capacity-reset] LEAGUE_CAPACITY_RESET_OVERRIDE_SECS={} active — \
             using {}s cadence instead of {:?}",
            s, s, cfg.capacity_reset_cadence
        );
    }

    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        // Under load, skip missed ticks rather than burst-catch-up. A slow
        // tick should be followed by one delayed tick, not N stacked ones.
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Skip the first immediate tick — let the rest of the system warm up.
        interval.tick().await;

        loop {
            interval.tick().await;
            match reset_capacities(&pool, &cfg).await {
                Ok(0) => { /* no-op tick — silent to keep logs clean */ }
                Ok(n) => info!("[capacity-reset] reset {n} protocol(s)"),
                Err(e) => error!("[capacity-reset] error: {e}"),
            }
        }
    });
}
