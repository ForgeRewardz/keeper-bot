// ============================================================
// quality_score.rs — Hourly per-protocol quality-score cron.
//
// Computes Q = 0.4·U + 0.3·R + 0.2·C + 0.1·S (devnet weights from
// league-config.md §quality-score). Each component is normalised to [0.0, 1.0]
// before weighting so the final score also lives in [0.0, 1.0] and fits
// `protocols.quality_score DECIMAL(5,4)`.
//
// Component definitions (MVP):
//   U — unique_wallets / 100, capped at 1.0 (baseline of 100 wallets =
//       "fully active" until governance tunes a network-wide denominator).
//   R — repeat_wallets / max(unique_wallets, 1), capped at 1.0.
//   C — successful_completions / max(attempts, 1). For MVP we don't yet
//       track Blink failures, so this is hard-coded to 1.0 with a TODO.
//   S — boolean fold of the two social milestones (follow_league,
//       launch_thread) → {0, 0.5, 1.0}.
//
// Components are logged per protocol on every tick so the score is observable
// without a DB query (`tracing` info-level).
// ============================================================

use sqlx::postgres::PgPool;
use sqlx::Row;
use std::time::Duration;
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info};

use crate::league_config::{load_league_config, LeagueConfig};

/// Baseline denominator for the unique-wallets component. 100 wallets →
/// U = 1.0. Lives here (not in LeagueConfig) until a network-tuning pass
/// promotes it; tracked in league-config.md §quality-score.
const UNIQUE_WALLETS_BASELINE: f64 = 100.0;

/// Placeholder for the C component until Blink failure-tracking lands
/// (see `score_one` TODO). Named constant so greps for "placeholder Q"
/// land here and the per-tick log can tag it.
const COMPLETION_SCORE_PLACEHOLDER: f64 = 1.0;

#[derive(Debug, Clone, Copy)]
struct Components {
    u: f64,
    r: f64,
    c: f64,
    s: f64,
}

impl Components {
    fn weighted(&self, cfg: &LeagueConfig) -> f64 {
        let q = cfg.quality_weights.u * self.u
            + cfg.quality_weights.r * self.r
            + cfg.quality_weights.c * self.c
            + cfg.quality_weights.s * self.s;
        q.clamp(0.0, 1.0)
    }
}

/// Compute U + R from point_events within the activity window.
async fn compute_engagement(
    pool: &PgPool,
    protocol_id: uuid::Uuid,
    activity_window_hours: u64,
    repeat_gap_hours: u64,
) -> sqlx::Result<(f64, f64)> {
    // Unique wallets in the window.
    let row = sqlx::query(
        r#"
        SELECT COUNT(DISTINCT user_wallet) AS unique_count
          FROM point_events
         WHERE protocol_id = $1
           AND created_at >= NOW() - make_interval(hours => $2)
        "#,
    )
    .bind(protocol_id)
    .bind(activity_window_hours as i64)
    .fetch_one(pool)
    .await?;
    let unique: i64 = row.try_get("unique_count")?;
    let u_component = (unique as f64 / UNIQUE_WALLETS_BASELINE).clamp(0.0, 1.0);

    if unique == 0 {
        return Ok((u_component, 0.0));
    }

    // Repeat wallets: those with 2+ events spaced > repeat_gap_hours apart.
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS repeat_count
          FROM (
            SELECT user_wallet
              FROM point_events
             WHERE protocol_id = $1
               AND created_at >= NOW() - make_interval(hours => $2)
             GROUP BY user_wallet
            HAVING (MAX(created_at) - MIN(created_at)) >= make_interval(hours => $3)
          ) repeats
        "#,
    )
    .bind(protocol_id)
    .bind(activity_window_hours as i64)
    .bind(repeat_gap_hours as i64)
    .fetch_one(pool)
    .await?;
    let repeats: i64 = row.try_get("repeat_count")?;
    let r_component = (repeats as f64 / unique as f64).clamp(0.0, 1.0);

    Ok((u_component, r_component))
}

/// Social fold from milestones.slug ∈ {follow_league, launch_thread}.
/// Scoped by protocol_id (migration 042) so admins owning multiple
/// protocols don't bleed social credits across them.
async fn compute_social(pool: &PgPool, protocol_id: uuid::Uuid) -> sqlx::Result<f64> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS social_count
          FROM rewardz_earnings re
          JOIN milestones m ON m.id = re.milestone_id
         WHERE re.protocol_id = $1
           AND m.bucket = 'social'
        "#,
    )
    .bind(protocol_id)
    .fetch_one(pool)
    .await?;
    let count: i64 = row.try_get("social_count")?;
    Ok((count as f64 / 2.0).clamp(0.0, 1.0))
}

async fn score_one(
    pool: &PgPool,
    cfg: &LeagueConfig,
    protocol_id: uuid::Uuid,
) -> sqlx::Result<()> {
    let (u, r) =
        compute_engagement(pool, protocol_id, cfg.activity_window_hours, cfg.repeat_gap_hours)
            .await?;
    // TODO(blink-failures): wire C to successful/attempted Blink calls once
    // failure-tracking lands. Holding at 1.0 means Q biases up — acceptable
    // for MVP since we'd rather not punish protocols for an unmeasured signal.
    let c = COMPLETION_SCORE_PLACEHOLDER;
    let s = compute_social(pool, protocol_id).await?;

    let comps = Components { u, r, c, s };
    let q = comps.weighted(cfg);

    info!(
        "[quality-score] protocol={} U={:.4} R={:.4} C={:.4} (placeholder) S={:.4} → Q={:.4}",
        protocol_id, u, r, c, s, q
    );

    sqlx::query(
        "UPDATE protocols SET quality_score = $1::numeric WHERE id = $2",
    )
    .bind(q)
    .bind(protocol_id)
    .execute(pool)
    .await?;

    Ok(())
}

async fn tick(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<u64> {
    // Score every active protocol. `visibility = 'hidden'` is intentionally
    // INCLUDED — a hidden protocol's score is still tracked so the visibility
    // tick (task 14) can decide whether to un-hide.
    let rows = sqlx::query("SELECT id FROM protocols WHERE status = 'active'")
        .fetch_all(pool)
        .await?;

    let mut scored = 0u64;
    for row in &rows {
        let id: uuid::Uuid = row.try_get("id")?;
        if let Err(e) = score_one(pool, cfg, id).await {
            error!("[quality-score] protocol={id} error: {e}");
            continue;
        }
        scored += 1;
    }
    Ok(scored)
}

/// Spawn the hourly cron loop.
pub fn start_quality_score_cron(interval_secs: u64, pool: PgPool) {
    let cfg = load_league_config();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // skip the immediate first tick

        loop {
            interval.tick().await;
            match tick(&pool, &cfg).await {
                Ok(n) => info!("[quality-score] scored {n} protocol(s)"),
                Err(e) => error!("[quality-score] tick error: {e}"),
            }
        }
    });
}
