// ============================================================
// anti_abuse.rs — hourly cron that scans active protocols for
// abuse signals and raises `abuse_flags` rows. The flag is the
// gate; downstream consumers (milestone-processor, visibility
// tick) read `abuse_flags WHERE resolved_at IS NULL`.
//
// Detection coverage in MVP:
//   - `repetitive_self_use`: the protocol's own admin wallet is
//     the dominant participant in its own point_events stream.
//     Heuristic: ≥10 events in last 24h AND self_share ≥ 0.5.
//     Severity scales by self_share (0.5–0.75 → low, ≥0.75 →
//     medium). High is reserved for human escalation.
//   - `unused_issuance`: protocol has held its full starter grant
//     for ≥7 days with zero downstream point_events. Indicates a
//     parked protocol claiming league inventory it never spends.
//     Always low severity (it's a soft signal).
//   - `daily_cap_breach` (kind='other' with bucket in evidence):
//     protocol's per-bucket daily count exceeded
//     `cfg.daily_caps.{self,social,referral}`. Only the `self`
//     bucket is enforced here today — see "out of scope" below
//     for why social and referral buckets are not.
//
// Out of scope this round (separately tracked):
//   - `blink_failure_rate`: requires Blink failure-tracking which
//     does not yet exist (see quality_score.rs TODO). Listed in
//     the migration enum so the column accepts it once we add it.
//   - `cluster_similarity`: requires wallet-graph analysis off
//     this service; future task owns it.
//   - `daily_caps.social`: the social-bucket awards (follow_league,
//     launch_thread) are no-ops in milestone_processor today
//     (admin-trigger path TODO), so there is no rewardz_earnings
//     row to count against the cap. Re-enable when those slugs
//     start firing.
//   - `daily_caps.referral`: the referral subsystem (task 21,
//     `POST /referrals/attribute`) is not yet built; no point_events
//     of a `referral` channel/type exist. Wire detection here once
//     task 21 lands and confirms the bucket key.
//
// Idempotency: each detector inserts at most one OPEN flag per
// (protocol_id, kind) by guarding on `NOT EXISTS (… resolved_at
// IS NULL)`. Resolved flags don't block a new flag — that's the
// re-flag path after ops clears the previous one.
// ============================================================

use sqlx::postgres::PgPool;
use std::time::Duration;
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info};

use crate::league_config::{load_league_config, LeagueConfig};

/// Minimum total events in the 24h window before the self-use ratio
/// is meaningful. Below this we'd flag every brand-new protocol
/// because the admin's smoke-test event is 100% of the dataset.
const SELF_USE_MIN_EVENTS: i64 = 10;

/// Self-share thresholds for repetitive_self_use severity. Lower
/// bound (inclusive) is the trigger; upper bound shifts severity.
const SELF_USE_LOW_THRESHOLD: f64 = 0.5;
const SELF_USE_MEDIUM_THRESHOLD: f64 = 0.75;

/// Days a starter grant can sit untouched before we flag the
/// protocol as parked.
const UNUSED_ISSUANCE_DAYS: i64 = 7;

async fn detect_repetitive_self_use(pool: &PgPool) -> sqlx::Result<u64> {
    // Single statement: insert one flag per protocol that crosses
    // the threshold AND has no open flag of this kind already. The
    // severity is computed inline from the ratio so a worsening
    // protocol doesn't get a duplicate row — it stays at the
    // severity recorded the first time we caught it. (Escalation
    // is a separate path: ops resolves the low and we'll re-flag
    // at whatever severity the next tick observes.)
    let result = sqlx::query(
        r#"
        INSERT INTO abuse_flags (protocol_id, kind, severity, evidence)
        SELECT
            p.id,
            'repetitive_self_use',
            CASE WHEN stats.self_share >= $2 THEN 'medium' ELSE 'low' END,
            jsonb_build_object(
                'window_hours', 24,
                'total_events', stats.total_events,
                'self_events', stats.self_events,
                'self_share', stats.self_share
            )
        FROM protocols p
        JOIN LATERAL (
            SELECT
                COUNT(*) AS total_events,
                COUNT(*) FILTER (WHERE pe.user_wallet = p.admin_wallet) AS self_events,
                CASE WHEN COUNT(*) = 0 THEN 0.0
                     ELSE COUNT(*) FILTER (WHERE pe.user_wallet = p.admin_wallet)::float8
                          / COUNT(*)::float8
                END AS self_share
            FROM point_events pe
            WHERE pe.protocol_id = p.id
              AND pe.created_at >= NOW() - INTERVAL '24 hours'
        ) stats ON TRUE
        WHERE p.status = 'active'
          AND stats.total_events >= $1
          AND stats.self_share >= $3
          AND NOT EXISTS (
            SELECT 1 FROM abuse_flags a
             WHERE a.protocol_id = p.id
               AND a.kind = 'repetitive_self_use'
               AND a.resolved_at IS NULL
          )
        "#,
    )
    .bind(SELF_USE_MIN_EVENTS)
    .bind(SELF_USE_MEDIUM_THRESHOLD)
    .bind(SELF_USE_LOW_THRESHOLD)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

async fn detect_unused_issuance(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<u64> {
    // A protocol is "parked" when its capacity window has been open
    // for ≥ UNUSED_ISSUANCE_DAYS, the remaining_capacity is still
    // exactly the starter grant (i.e. nothing has accrued against
    // it), AND the protocol has issued zero point_events ever. The
    // last clause is what distinguishes parked-and-idle from
    // active-but-thrifty.
    let baseline = cfg.starter_grant_rewardz as i64;
    let result = sqlx::query(
        r#"
        INSERT INTO abuse_flags (protocol_id, kind, severity, evidence)
        SELECT
            p.id,
            'unused_issuance',
            'low',
            jsonb_build_object(
                'days_since_window_start',
                EXTRACT(EPOCH FROM (NOW() - p.capacity_window_start)) / 86400.0,
                'remaining_capacity', p.remaining_capacity,
                'starter_grant', $1::bigint
            )
        FROM protocols p
        WHERE p.status = 'active'
          AND p.capacity_window_start IS NOT NULL
          AND p.capacity_window_start <= NOW() - make_interval(days => $2)
          AND p.remaining_capacity = $1
          AND NOT EXISTS (
            SELECT 1 FROM point_events pe WHERE pe.protocol_id = p.id
          )
          AND NOT EXISTS (
            SELECT 1 FROM abuse_flags a
             WHERE a.protocol_id = p.id
               AND a.kind = 'unused_issuance'
               AND a.resolved_at IS NULL
          )
        "#,
    )
    .bind(baseline)
    .bind(UNUSED_ISSUANCE_DAYS)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Detect protocols whose admin_wallet self-awarded point_events in
/// the last 24h exceed `cfg.daily_caps.self`. Raises an abuse_flag
/// of kind='daily_cap_breach' (added in migration 044) with the
/// bucket name in evidence so social/referral can share the kind
/// once they are wired up. The (protocol_id, kind) partial UNIQUE
/// from migration 043 enforces one open daily-cap flag per protocol;
/// re-flagging another bucket on the same protocol must wait for
/// the previous flag to be resolved (acceptable: ops sees one
/// banner, fixes the underlying behaviour, clears the flag).
async fn detect_daily_cap_self(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<u64> {
    let cap = cfg.daily_caps.self_ as i64;
    let result = sqlx::query(
        r#"
        INSERT INTO abuse_flags (protocol_id, kind, severity, evidence)
        SELECT
            p.id,
            'daily_cap_breach',
            'medium',
            jsonb_build_object(
                'bucket', 'self',
                'window_hours', 24,
                'count', stats.self_count,
                'cap', $1::bigint
            )
        FROM protocols p
        JOIN LATERAL (
            SELECT COUNT(*) AS self_count
              FROM point_events pe
             WHERE pe.protocol_id = p.id
               AND pe.user_wallet = p.admin_wallet
               AND pe.created_at >= NOW() - INTERVAL '24 hours'
        ) stats ON TRUE
        WHERE p.status = 'active'
          AND stats.self_count > $1
          AND NOT EXISTS (
            SELECT 1 FROM abuse_flags a
             WHERE a.protocol_id = p.id
               AND a.kind = 'daily_cap_breach'
               AND a.resolved_at IS NULL
          )
        "#,
    )
    .bind(cap)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

async fn tick(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<u64> {
    // Per-detector errors are logged here so a failing detector doesn't
    // mask the others. Successes only update the counter — the outer
    // cron loop owns the single per-tick info log line, matching the
    // log shape of milestone_processor / quality_score / capacity_reset.
    let mut total = 0u64;
    match detect_repetitive_self_use(pool).await {
        Ok(n) => total += n,
        Err(e) => error!("[anti-abuse] repetitive_self_use error: {}", e),
    }
    match detect_unused_issuance(pool, cfg).await {
        Ok(n) => total += n,
        Err(e) => error!("[anti-abuse] unused_issuance error: {}", e),
    }
    match detect_daily_cap_self(pool, cfg).await {
        Ok(n) => total += n,
        Err(e) => error!("[anti-abuse] daily_cap_self error: {}", e),
    }
    Ok(total)
}

/// Spawn the anti-abuse cron. Default cadence hourly (matches
/// `ANTI_ABUSE_INTERVAL_SECS`). Cheap query — no external IO.
pub fn start_anti_abuse_cron(interval_secs: u64, pool: PgPool) {
    let cfg = load_league_config();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // skip immediate first tick

        loop {
            interval.tick().await;
            match tick(&pool, &cfg).await {
                Ok(0) => { /* nothing to flag — silent */ }
                Ok(n) => info!("[anti-abuse] raised {n} flag(s)"),
                Err(e) => error!("[anti-abuse] tick error: {e}"),
            }
        }
    });
}
