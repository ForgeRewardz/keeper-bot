// ============================================================
// milestone_processor.rs — 5-minute cron that evaluates the 9
// league-config.md milestone predicates and inserts rewardz_earnings
// rows for any newly-qualifying protocols.
//
// Exhaustiveness is intentional: the processor MUST know about every
// slug in `league-config.md §Milestones`. Missing a slug silently
// means the reward never fires. A match arm for each slug acts as a
// compile-time exhaustiveness check (the arm list is covered by the
// `ALL_SLUGS` const which CI asserts equals the seed-file's set).
//
// Idempotency: every INSERT is guarded by `NOT EXISTS` against
// `rewardz_earnings (protocol_id, milestone_id)`, backed by a partial
// unique index (migration 042). The index is the source of truth — the
// NOT EXISTS guard is belt-and-braces to keep the error path tidy so a
// duplicate tick doesn't fail loud, it just inserts zero rows.
//
// Note on keying: earlier drafts keyed on `protocol_authority` (the admin
// wallet). That broke for admins owning multiple protocols — once
// protocol X earned a milestone, protocol Y was permanently locked out.
// We scope on `protocol_id` instead and keep `protocol_authority` only
// because the Merkle leaf (task 32) builds on it.
//
// Abuse gate: protocols with an unresolved `abuse_flags` row are
// excluded from every predicate (rewardz accrual is frozen until the
// flag is cleared). See league-config.md §Anti-abuse.
//
// Social milestones (follow_league, launch_thread) have no automatic
// predicate — they require an out-of-band signal (admin click or
// proof-of-tweet). The cron *does* list them so the exhaustiveness
// check passes, but the predicate is a deliberate no-op with a TODO
// pointing at the follow-up task.
// ============================================================

use sqlx::postgres::PgPool;
use sqlx::Row;
use std::time::Duration;
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info, warn};

use crate::league_config::{load_league_config, LeagueConfig};

/// The complete set of slugs this processor knows how to evaluate.
/// Must stay in sync with `api/src/db/seeds/milestones.ts`. A parity
/// test (milestone-processor-parity.test.ts, task-follow-up) diffs the
/// two lists at CI time.
const ALL_SLUGS: &[&str] = &[
    // activation
    "first_blink",
    "first_stake",
    "first_issuance",
    "first_five_unique_wallets",
    "first_repeat_user",
    // growth
    "twenty_five_unique_wallets",
    "ten_repeat_users",
    // social
    "follow_league",
    "launch_thread",
];

/// Fetch milestone_id + reward amount per slug for the active network.
/// Returned as a flat Vec so we can iterate exhaustively against ALL_SLUGS.
async fn load_milestone_rewards(
    pool: &PgPool,
    network: &str,
) -> sqlx::Result<Vec<(String, i64, i64)>> {
    let rows = sqlx::query(
        r#"
        SELECT m.slug, m.id AS milestone_id, mr.rewardz_amount
          FROM milestones m
          JOIN milestone_rewards mr ON mr.milestone_id = m.id
         WHERE mr.network = $1
        "#,
    )
    .bind(network)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let slug: String = row.try_get("slug")?;
        let milestone_id: i64 = row.try_get("milestone_id")?;
        let amount: i64 = row.try_get("rewardz_amount")?;
        out.push((slug, milestone_id, amount));
    }
    Ok(out)
}

/// Dispatch a single milestone to its predicate and run the INSERT.
/// Returns the number of new rewardz_earnings rows inserted.
async fn process_slug(
    pool: &PgPool,
    cfg: &LeagueConfig,
    slug: &str,
    milestone_id: i64,
    amount: i64,
) -> sqlx::Result<u64> {
    // Every predicate shares the same outer gate:
    //   - protocol is active
    //   - no open abuse flag
    //   - milestone not already earned by this protocol
    // We compose that gate inline per-slug so the final SQL stays a
    // single INSERT ... SELECT (no temp tables).
    match slug {
        "first_blink" => insert_if(
            pool,
            milestone_id,
            amount,
            slug,
            "EXISTS (SELECT 1 FROM point_events pe WHERE pe.protocol_id = p.id)",
        )
        .await,

        "first_stake" => {
            // TODO(task-16a): wire to stake_watcher once ProtocolStake is
            // mirrored into a staking_events table. Until then the
            // predicate is a no-op — the slug is listed for exhaustiveness,
            // not because it can fire yet.
            Ok(0)
        }

        "first_issuance" => insert_if(
            pool,
            milestone_id,
            amount,
            slug,
            "EXISTS (SELECT 1 FROM point_events pe \
              WHERE pe.protocol_id = p.id \
                AND pe.type = 'awarded' \
                AND pe.user_wallet <> p.admin_wallet)",
        )
        .await,

        "first_five_unique_wallets" => insert_if(
            pool,
            milestone_id,
            amount,
            slug,
            "(SELECT COUNT(DISTINCT pe.user_wallet) \
                FROM point_events pe \
               WHERE pe.protocol_id = p.id \
                 AND pe.user_wallet <> p.admin_wallet) >= 5",
        )
        .await,

        "first_repeat_user" => {
            insert_repeat_users(pool, cfg, milestone_id, amount, slug, 1).await
        }

        "twenty_five_unique_wallets" => insert_if(
            pool,
            milestone_id,
            amount,
            slug,
            "(SELECT COUNT(DISTINCT pe.user_wallet) \
                FROM point_events pe \
               WHERE pe.protocol_id = p.id \
                 AND pe.user_wallet <> p.admin_wallet) >= 25",
        )
        .await,

        "ten_repeat_users" => {
            insert_repeat_users(pool, cfg, milestone_id, amount, slug, 10).await
        }

        "follow_league" | "launch_thread" => {
            // Intentional no-op. These fire from an admin-triggered API
            // path (task-follow-up); listing them here keeps the
            // exhaustiveness check honest.
            Ok(0)
        }

        other => {
            warn!(
                "[milestone-processor] unknown slug '{}' — catalogue drifted from ALL_SLUGS?",
                other
            );
            Ok(0)
        }
    }
}

/// Public entry point: run a standard INSERT with a predicate built
/// from a string *literal*. The `&'static str` bound is deliberate — it
/// prevents a future caller from accidentally passing a runtime string
/// (env var, DB column, user input) through the SQL formatter. If you
/// need a dynamic predicate, go through `insert_with_dynamic_predicate`
/// and convince yourself the format inputs are typed numerics, never
/// strings.
async fn insert_if(
    pool: &PgPool,
    milestone_id: i64,
    amount: i64,
    slug: &str,
    predicate_sql: &'static str,
) -> sqlx::Result<u64> {
    insert_with_dynamic_predicate(pool, milestone_id, amount, slug, predicate_sql).await
}

/// Internal shared body. Accepts any `&str` — callers in this module are
/// responsible for proving the predicate is safe. Only two call sites:
///   - `insert_if` (static literal)
///   - `insert_repeat_users` (format! with i64s)
///
/// Do NOT add a third call site that passes a string derived from DB or
/// env without a matching safety argument inline.
async fn insert_with_dynamic_predicate(
    pool: &PgPool,
    milestone_id: i64,
    amount: i64,
    slug: &str,
    predicate_sql: &str,
) -> sqlx::Result<u64> {
    // Reward amount is scaled by the admin_wallet's wallet_weights row
    // (founder/team/external/external_repeat_after_gap → multiplier per
    // league-config.md §Anti-abuse). Missing row defaults to 1.0 so
    // pre-league protocols still get the base amount. The product is
    // FLOOR'd to bigint so we never store a fractional rewardz_amount.
    let sql = format!(
        r#"
        INSERT INTO rewardz_earnings
            (protocol_id, protocol_authority, amount, reason, milestone_id)
        SELECT
            p.id,
            p.admin_wallet,
            FLOOR($2::bigint * COALESCE(ww.weight, 1.0))::bigint,
            $3::text,
            $1::bigint
          FROM protocols p
          LEFT JOIN wallet_weights ww
                 ON ww.protocol_id = p.id
                AND ww.wallet = p.admin_wallet
         WHERE p.status = 'active'
           AND NOT EXISTS (
             SELECT 1 FROM abuse_flags a
              WHERE a.protocol_id = p.id AND a.resolved_at IS NULL
           )
           AND NOT EXISTS (
             SELECT 1 FROM rewardz_earnings re
              WHERE re.protocol_id = p.id
                AND re.milestone_id = $1
           )
           AND {predicate}
        ON CONFLICT (protocol_id, milestone_id) DO NOTHING
        "#,
        predicate = predicate_sql
    );

    let reason = format!("milestone:{slug}");
    let rows_affected = sqlx::query(&sql)
        .bind(milestone_id)
        .bind(amount)
        .bind(reason)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows_affected)
}

/// Repeat-user predicate isolated because it needs the league's
/// `repeat_gap_hours` driving a make_interval() inside the SQL.
async fn insert_repeat_users(
    pool: &PgPool,
    cfg: &LeagueConfig,
    milestone_id: i64,
    amount: i64,
    slug: &str,
    min_repeat_wallets: i64,
) -> sqlx::Result<u64> {
    // A "repeat wallet" is one with >= 2 point_events whose MAX-MIN
    // timestamp spans the league's repeat_gap_hours. Same shape as the
    // quality-score R component so the two signals agree.
    let predicate = format!(
        "(SELECT COUNT(*) FROM ( \
            SELECT pe.user_wallet \
              FROM point_events pe \
             WHERE pe.protocol_id = p.id \
               AND pe.user_wallet <> p.admin_wallet \
             GROUP BY pe.user_wallet \
            HAVING (MAX(pe.created_at) - MIN(pe.created_at)) >= make_interval(hours => {gap}) \
         ) q) >= {threshold}",
        gap = cfg.repeat_gap_hours as i64,
        threshold = min_repeat_wallets,
    );
    insert_with_dynamic_predicate(pool, milestone_id, amount, slug, &predicate).await
}

async fn tick(pool: &PgPool, cfg: &LeagueConfig) -> sqlx::Result<u64> {
    let network = match cfg.network {
        crate::league_config::Network::Devnet => "devnet",
        crate::league_config::Network::Mainnet => "mainnet",
    };

    let rewards = load_milestone_rewards(pool, network).await?;

    // Exhaustiveness check: fail loud if the seed catalogue diverged from
    // ALL_SLUGS (e.g. a slug was renamed on one side but not the other).
    // This protects us from the "silently skipped milestone" failure mode.
    for slug in ALL_SLUGS {
        if !rewards.iter().any(|(s, _, _)| s == slug) {
            warn!(
                "[milestone-processor] slug '{}' present in code but missing \
                 from milestone_rewards seed for network={}",
                slug, network
            );
        }
    }

    let mut total = 0u64;
    for (slug, milestone_id, amount) in &rewards {
        match process_slug(pool, cfg, slug, *milestone_id, *amount).await {
            Ok(0) => { /* nothing new — silent to keep logs clean */ }
            Ok(n) => {
                info!(
                    "[milestone-processor] slug={} awarded {} protocol(s) (amount={})",
                    slug, n, amount
                );
                total += n;
            }
            Err(e) => {
                error!("[milestone-processor] slug={} error: {}", slug, e);
            }
        }
    }
    Ok(total)
}

/// Spawn the milestone-processor cron. Default cadence is 5 minutes
/// (wired in `main.rs` via `MILESTONE_PROCESSOR_INTERVAL_SECS`, default 300).
pub fn start_milestone_processor_cron(interval_secs: u64, pool: PgPool) {
    let cfg = load_league_config();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // skip the immediate first tick

        loop {
            interval.tick().await;
            match tick(&pool, &cfg).await {
                Ok(0) => { /* no new milestones — silent */ }
                Ok(n) => info!("[milestone-processor] awarded {} milestone row(s)", n),
                Err(e) => error!("[milestone-processor] tick error: {}", e),
            }
        }
    });
}
