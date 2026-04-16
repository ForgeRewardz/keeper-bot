// ============================================================
// leaderboard.rs — daily league leaderboard snapshot + bonus
// rewardz issuance.
//
// On each tick (default daily) the cron:
//   1. Computes a per-protocol ranking signal from the league's
//      ranking_weights (quality, unique_wallets, repeat_users,
//      completions). Each component is normalised to [0, 1] before
//      being multiplied by its weight, mirroring the shape of the
//      quality-score components.
//   2. Selects the top-N (cfg.leaderboard_top_n) and writes one
//      `league_leaderboard_snapshots` row per protocol with rank,
//      raw counts, the bonus amount, and featured=true.
//   3. Awards `leaderboard_bonus_rewardz` to each top-N protocol
//      via a `rewardz_earnings` row tagged
//      `reason='leaderboard:<snapshot_date>'`. The earnings row
//      has `milestone_id = NULL` so the partial unique index
//      (migration 042) does not constrain it — leaderboard bonuses
//      can repeat across snapshot dates.
//
// Idempotency: snapshots are uniqued on `(snapshot_date, rank)`
// and `(snapshot_date, protocol_id)` (migration 040). The cron
// inserts with `ON CONFLICT DO NOTHING` so a re-run on the same
// day is a no-op for snapshot rows. Bonus rewardz_earnings rows
// are uniqued on `(protocol_id, reason) WHERE milestone_id IS NULL`
// (migration 045) with `ON CONFLICT DO NOTHING`, so the database
// enforces at-most-one-bonus-per-snapshot-date even under
// concurrent ticks. The `reason` string is derived from the
// snapshot row's `snapshot_date` column in SQL rather than
// computed in Rust, so there is no way for the Rust-side date and
// the Postgres-side `CURRENT_DATE` to drift apart (which would
// have been possible under a non-UTC DB session timezone).
//
// Scope filter: only `status='active'` AND `visibility != 'hidden'`
// protocols are eligible. Hidden protocols can't earn the bonus
// even if their underlying counts would have placed them. at_risk
// protocols ARE eligible — the visibility downrank lives in the
// ranking-engine for the discovery surface, not here.
// ============================================================

use sqlx::postgres::PgPool;
use std::time::Duration;
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info};

use crate::league_config::{load_league_config, LeagueConfig};

/// Activity window for the engagement components of the ranking
/// signal. Mirrors the quality-score window so the two surfaces
/// agree on what "recent" means.
const RANKING_WINDOW_HOURS: i64 = 168;

/// Normalisation baselines for the engagement components. unique
/// wallets / repeat users are divided by these to produce a [0, 1]
/// component before applying the league ranking weight. Same shape
/// as quality_score.rs::UNIQUE_WALLETS_BASELINE.
const UNIQUE_WALLETS_BASELINE: f64 = 100.0;
const REPEAT_USERS_BASELINE: f64 = 50.0;
const COMPLETIONS_BASELINE: f64 = 200.0;

async fn write_snapshot_and_bonuses(
    pool: &PgPool,
    cfg: &LeagueConfig,
) -> sqlx::Result<u64> {
    let top_n = cfg.leaderboard_top_n as i64;
    let bonus = cfg.leaderboard_bonus_rewardz as i64;

    // Single query computes per-protocol counts, weighted score,
    // and rank, then inserts the top-N. We do the work in a CTE so
    // the same `ranked` set drives both the snapshot insert and
    // (in a follow-up statement) the bonus earnings insert.
    let snapshot_rows = sqlx::query(
        r#"
        WITH per_protocol AS (
            SELECT
                p.id AS protocol_id,
                COALESCE(p.quality_score, 0)::float8 AS quality_score,
                COUNT(DISTINCT pe.user_wallet)
                    FILTER (WHERE pe.user_wallet <> p.admin_wallet) AS unique_wallets,
                (
                    SELECT COUNT(*) FROM (
                        SELECT user_wallet
                          FROM point_events pe2
                         WHERE pe2.protocol_id = p.id
                           AND pe2.user_wallet <> p.admin_wallet
                           AND pe2.created_at >= NOW() - make_interval(hours => $3)
                         GROUP BY user_wallet
                        HAVING (MAX(pe2.created_at) - MIN(pe2.created_at)) >= INTERVAL '24 hours'
                    ) r
                ) AS repeat_users,
                COUNT(*) FILTER (
                    WHERE pe.type = 'awarded'
                      AND pe.user_wallet <> p.admin_wallet
                ) AS completions
            FROM protocols p
            LEFT JOIN point_events pe
                   ON pe.protocol_id = p.id
                  AND pe.created_at >= NOW() - make_interval(hours => $3)
            WHERE p.status = 'active'
              AND p.visibility <> 'hidden'
            GROUP BY p.id
        ),
        scored AS (
            SELECT
                protocol_id,
                unique_wallets,
                repeat_users,
                completions,
                (
                    $4::float8 * LEAST(quality_score, 1.0)
                  + $5::float8 * LEAST(unique_wallets::float8 / $8::float8, 1.0)
                  + $6::float8 * LEAST(repeat_users::float8 / $9::float8, 1.0)
                  + $7::float8 * LEAST(completions::float8 / $10::float8, 1.0)
                ) AS score
            FROM per_protocol
        ),
        ranked AS (
            SELECT
                protocol_id,
                unique_wallets,
                repeat_users,
                completions,
                score,
                ROW_NUMBER() OVER (ORDER BY score DESC, protocol_id ASC) AS rk
            FROM scored
        )
        INSERT INTO league_leaderboard_snapshots
            (snapshot_date, rank, protocol_id, unique_wallets,
             repeat_users, successful_completions, bonus_awarded, featured)
        SELECT
            CURRENT_DATE,
            rk::int,
            protocol_id,
            unique_wallets,
            repeat_users,
            completions,
            $2::bigint,
            true
          FROM ranked
         WHERE rk <= $1
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(top_n)
    .bind(bonus)
    .bind(RANKING_WINDOW_HOURS)
    .bind(cfg.ranking_weights.quality as f64)
    .bind(cfg.ranking_weights.unique_wallets as f64)
    .bind(cfg.ranking_weights.repeat_users as f64)
    .bind(cfg.ranking_weights.completions as f64)
    .bind(UNIQUE_WALLETS_BASELINE)
    .bind(REPEAT_USERS_BASELINE)
    .bind(COMPLETIONS_BASELINE)
    .execute(pool)
    .await?
    .rows_affected();

    if snapshot_rows == 0 {
        // Either no eligible protocols, or today's snapshot is
        // already in place (idempotent re-run). Skip the bonus
        // insert too — the NOT EXISTS guard would no-op anyway,
        // but skipping saves a query.
        return Ok(0);
    }

    // Award the bonus to every protocol whose snapshot row was
    // created today. Source-of-truth is the snapshot table so the
    // bonus row count always equals the snapshot row count, even
    // if the ranking signal shifts mid-day. The reason string is
    // derived from `s.snapshot_date` in SQL so it cannot drift
    // from the snapshot row it is paired with, and the partial
    // unique index from migration 045 makes the insert race-safe.
    let bonus_rows = sqlx::query(
        r#"
        INSERT INTO rewardz_earnings
            (protocol_id, protocol_authority, amount, reason, milestone_id)
        SELECT
            s.protocol_id,
            p.admin_wallet,
            $1::bigint,
            'leaderboard:' || s.snapshot_date::text,
            NULL
          FROM league_leaderboard_snapshots s
          JOIN protocols p ON p.id = s.protocol_id
         WHERE s.snapshot_date = CURRENT_DATE
        ON CONFLICT (protocol_id, reason) WHERE milestone_id IS NULL
        DO NOTHING
        "#,
    )
    .bind(bonus)
    .execute(pool)
    .await?
    .rows_affected();

    info!(
        "[leaderboard] snapshot rows={} bonus_rows={} bonus_amount={}",
        snapshot_rows, bonus_rows, bonus
    );

    Ok(snapshot_rows)
}

async fn tick(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<u64> {
    write_snapshot_and_bonuses(pool, cfg).await
}

/// Spawn the leaderboard cron. Default cadence daily
/// (`LEADERBOARD_INTERVAL_SECS=86400`). Runs once per cadence; the
/// snapshot itself is keyed on CURRENT_DATE so even if the interval
/// fires twice in the same UTC day the second run is a no-op via
/// the snapshot UNIQUE constraint + bonus NOT EXISTS guard.
pub fn start_leaderboard_cron(interval_secs: u64, pool: PgPool) {
    let cfg = load_league_config();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // skip immediate first tick

        loop {
            interval.tick().await;
            match tick(&pool, &cfg).await {
                Ok(0) => { /* already snapshotted today — silent */ }
                Ok(n) => info!("[leaderboard] wrote {n} snapshot row(s)"),
                Err(e) => error!("[leaderboard] tick error: {e}"),
            }
        }
    });
}
