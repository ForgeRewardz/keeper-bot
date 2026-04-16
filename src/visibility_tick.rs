// ============================================================
// visibility_tick.rs — 15-minute cron that drives the
// `protocols.visibility` state machine (active|at_risk|hidden).
//
// Trigger matrix per league-config.md §Visibility:
//
//   to hidden:
//     - inactivity ≥ inactivity_window_hours, OR
//     - any open abuse_flag with severity = 'high'
//
//   to at_risk (when not hiding):
//     - capacity_pct ≤ first warning threshold
//       (cfg.capacity_warning_thresholds[0]), OR
//     - any open abuse_flag (low/medium), OR
//     - inactivity ≥ activity_window_hours (but not yet hidden), OR
//     - Blink-health below threshold (placeholder: quality_score
//       below BLINK_HEALTH_QUALITY_FLOOR — see note below).
//
//   to active (recovery):
//     - none of the above signals fire AND there is recent
//       activity within activity_window_hours.
//
// Blink-health note: real Blink failure tracking is not yet in
// place (see quality_score.rs C-component TODO). Until it is,
// the C component is hard-coded to 1.0 and quality_score therefore
// reflects only the U/R/S signals. Wiring the trigger now means
// the visibility state machine is structurally complete; it will
// start firing meaningfully the moment Blink failure data flows
// into the C component. Treating this as a stub keeps SC4
// (visibility transitions for the four named triggers) defensible.
//
// "Recent activity" = MAX(point_events.created_at) for the
// protocol. A protocol with zero events ever is treated as
// having infinite inactivity — newcomers therefore land in
// at_risk immediately, which is intentional: they need to
// either earn an event or get hidden after the inactivity
// window. The console shows them an at_risk badge so the
// founder knows to act.
//
// Each transition writes ONE protocol_events row so the console
// timeline shows what changed. Flapping is dampened only by the
// 15-minute cadence; a follow-up could add hysteresis if it
// becomes noisy.
// ============================================================

use sqlx::postgres::PgPool;
use sqlx::Row;
use std::time::Duration;
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info};

use crate::league_config::{load_league_config, LeagueConfig};

/// Blink-health threshold expressed against `protocols.quality_score`.
/// quality_score lives in [0.0, 1.0]; we flag anything ≤ this as
/// at_risk on the Blink-health axis. Set conservatively low so that
/// while quality_score's C component is still a 1.0 placeholder this
/// trigger does not fire spuriously. Once Blink failure data feeds C,
/// real degradation will pull Q below the floor and the trigger
/// activates without further code changes.
const BLINK_HEALTH_QUALITY_FLOOR: f64 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Active,
    AtRisk,
    Hidden,
}

impl Visibility {
    fn from_db(s: &str) -> Self {
        match s {
            "active" => Visibility::Active,
            "at_risk" => Visibility::AtRisk,
            "hidden" => Visibility::Hidden,
            other => {
                // Defensive: an unknown value means the enum was extended
                // and this code wasn't updated. Treat as at_risk so the
                // protocol isn't silently promoted to active.
                tracing::warn!("[visibility-tick] unknown visibility '{}', treating as at_risk", other);
                Visibility::AtRisk
            }
        }
    }

    fn as_db(self) -> &'static str {
        match self {
            Visibility::Active => "active",
            Visibility::AtRisk => "at_risk",
            Visibility::Hidden => "hidden",
        }
    }

    fn level(self) -> &'static str {
        match self {
            Visibility::Active => "info",
            Visibility::AtRisk => "warning",
            Visibility::Hidden => "critical",
        }
    }
}

/// One row's worth of decision inputs. Pulled in a single SQL
/// pass so we don't N+1 the database.
struct Snapshot {
    protocol_id: uuid::Uuid,
    current: Visibility,
    capacity_pct: f64,
    hours_since_last_event: Option<f64>,
    open_high_flag: bool,
    open_any_flag: bool,
    quality_score: f64,
}

fn decide(snap: &Snapshot, cfg: &LeagueConfig) -> Visibility {
    let inactivity = snap
        .hours_since_last_event
        .unwrap_or(f64::INFINITY);

    if inactivity >= cfg.inactivity_window_hours as f64 || snap.open_high_flag {
        return Visibility::Hidden;
    }

    // First-warning threshold drives the at_risk capacity gate. If the
    // config ever ships an empty thresholds array (mainnet placeholder
    // mode, future tweak) we fall back to NEG_INFINITY so the capacity
    // gate is permanently false rather than panicking the cron task.
    let warning_threshold = cfg
        .capacity_warning_thresholds
        .first()
        .copied()
        .unwrap_or(f64::NEG_INFINITY);
    let blink_health_unhealthy = snap.quality_score <= BLINK_HEALTH_QUALITY_FLOOR;
    let at_risk = snap.capacity_pct <= warning_threshold
        || snap.open_any_flag
        || inactivity >= cfg.activity_window_hours as f64
        || blink_health_unhealthy;

    if at_risk { Visibility::AtRisk } else { Visibility::Active }
}

async fn load_snapshots(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<Vec<Snapshot>> {
    // capacity_pct: NULL remaining_capacity (never bootstrapped)
    // is treated as 1.0 — the protocol hasn't been given a chance
    // to spend yet. Once the capacity-reset cron initialises the
    // window, this becomes a real ratio.
    let baseline = cfg.starter_grant_rewardz as f64;
    let rows = sqlx::query(
        r#"
        SELECT
            p.id,
            p.visibility::text AS visibility,
            CASE
              WHEN p.remaining_capacity IS NULL THEN 1.0::float8
              WHEN $1 = 0 THEN 0.0::float8
              ELSE LEAST(p.remaining_capacity::float8 / $1::float8, 1.0::float8)
            END AS capacity_pct,
            EXTRACT(EPOCH FROM (NOW() - (
                SELECT MAX(pe.created_at) FROM point_events pe WHERE pe.protocol_id = p.id
            ))) / 3600.0 AS hours_since_last_event,
            EXISTS (
                SELECT 1 FROM abuse_flags a
                 WHERE a.protocol_id = p.id
                   AND a.resolved_at IS NULL
                   AND a.severity = 'high'
            ) AS open_high_flag,
            EXISTS (
                SELECT 1 FROM abuse_flags a
                 WHERE a.protocol_id = p.id
                   AND a.resolved_at IS NULL
            ) AS open_any_flag,
            COALESCE(p.quality_score, 1.0)::float8 AS quality_score
        FROM protocols p
        WHERE p.status = 'active'
        "#,
    )
    .bind(baseline)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let visibility: String = row.try_get("visibility")?;
        out.push(Snapshot {
            protocol_id: row.try_get("id")?,
            current: Visibility::from_db(&visibility),
            capacity_pct: row.try_get("capacity_pct")?,
            // Option<f64> distinguishes "no events ever" (NULL) from a
            // schema-drift type error (which surfaces via `?` here).
            hours_since_last_event: row.try_get::<Option<f64>, _>("hours_since_last_event")?,
            open_high_flag: row.try_get("open_high_flag")?,
            open_any_flag: row.try_get("open_any_flag")?,
            quality_score: row.try_get("quality_score")?,
        });
    }
    Ok(out)
}

async fn apply_transition(
    pool: &PgPool,
    snap: &Snapshot,
    next: Visibility,
) -> sqlx::Result<()> {
    // Two writes in a tx so the timeline event is never out of
    // sync with the column. Keeps `/protocols/:id/events` honest.
    let mut tx = pool.begin().await?;

    sqlx::query("UPDATE protocols SET visibility = $1::visibility_enum WHERE id = $2")
        .bind(next.as_db())
        .bind(snap.protocol_id)
        .execute(&mut *tx)
        .await?;

    let payload = serde_json::json!({
        "from": snap.current.as_db(),
        "to": next.as_db(),
        "capacity_pct": snap.capacity_pct,
        "hours_since_last_event": snap.hours_since_last_event,
        "open_high_flag": snap.open_high_flag,
        "open_any_flag": snap.open_any_flag,
        "quality_score": snap.quality_score,
    });

    sqlx::query(
        "INSERT INTO protocol_events (protocol_id, kind, level, payload) \
         VALUES ($1, 'visibility_change', $2, $3)",
    )
    .bind(snap.protocol_id)
    .bind(next.level())
    .bind(payload)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

async fn tick(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<u64> {
    let snapshots = load_snapshots(pool, cfg).await?;
    let mut transitions = 0u64;
    for snap in &snapshots {
        let next = decide(snap, cfg);
        if next == snap.current {
            continue;
        }
        match apply_transition(pool, snap, next).await {
            Ok(()) => {
                info!(
                    "[visibility-tick] protocol={} {} → {} (capacity_pct={:.3}, inactivity_h={:?}, high_flag={}, any_flag={})",
                    snap.protocol_id,
                    snap.current.as_db(),
                    next.as_db(),
                    snap.capacity_pct,
                    snap.hours_since_last_event,
                    snap.open_high_flag,
                    snap.open_any_flag,
                );
                transitions += 1;
            }
            Err(e) => {
                error!(
                    "[visibility-tick] protocol={} transition to {} failed: {}",
                    snap.protocol_id,
                    next.as_db(),
                    e
                );
            }
        }
    }
    Ok(transitions)
}

/// Spawn the visibility-tick cron. Default cadence 15 minutes
/// (`VISIBILITY_TICK_INTERVAL_SECS=900`).
pub fn start_visibility_tick_cron(interval_secs: u64, pool: PgPool) {
    let cfg = load_league_config();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // skip immediate first tick

        loop {
            interval.tick().await;
            match tick(&pool, &cfg).await {
                Ok(0) => { /* steady state — silent */ }
                Ok(n) => info!("[visibility-tick] {n} transition(s) applied"),
                Err(e) => error!("[visibility-tick] tick error: {e}"),
            }
        }
    });
}
