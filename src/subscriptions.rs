// ============================================================
// subscriptions.rs — Subscription executor cron
// ============================================================

use sqlx::postgres::PgPool;
use sqlx::Row;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{error, info, warn};

use crate::config::Config;

/// A subscription row from the database.
struct SubscriptionRow {
    subscription_id: String,
    wallet_address: String,
    intent_query: String,
    auto_execute: bool,
    streak_current: i64,
    streak_longest: i64,
    #[allow(dead_code)]
    multiplier: f64,
    frequency: String,
}

/// Streak multiplier tiers.
fn streak_multiplier(streak: i64) -> f64 {
    match streak {
        0..=2 => 1.0,
        3..=6 => 1.2,
        7..=13 => 1.5,
        14..=29 => 1.8,
        _ => 2.0,
    }
}

/// Poll for due subscriptions and execute them.
pub async fn poll_subscriptions(
    pool: &PgPool,
    config: &Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Polling for due subscriptions");

    // Query subscriptions that are due.
    let rows = sqlx::query(
        r#"
        SELECT subscription_id, wallet_address, intent_query, auto_execute,
               streak_current, streak_longest, multiplier, frequency
        FROM subscriptions
        WHERE next_due_at <= NOW() AND status = 'active'
        ORDER BY next_due_at ASC
        LIMIT 100
        "#,
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        info!("No due subscriptions");
        return Ok(());
    }

    let due_subs: Vec<SubscriptionRow> = rows
        .iter()
        .map(|r| SubscriptionRow {
            subscription_id: r.get("subscription_id"),
            wallet_address: r.get("wallet_address"),
            intent_query: r.get("intent_query"),
            auto_execute: r.get("auto_execute"),
            streak_current: r.get("streak_current"),
            streak_longest: r.get("streak_longest"),
            multiplier: r.get("multiplier"),
            frequency: r.get("frequency"),
        })
        .collect();

    info!("Found {} due subscription(s)", due_subs.len());

    let client = reqwest::Client::new();

    for sub in &due_subs {
        let sub_id = &sub.subscription_id;
        info!("Processing subscription {sub_id}");

        // Call API to resolve intent (internal key auth for service-to-service).
        let resolve_url = format!("{}/v1/intents/resolve", config.api_base_url);
        let resolve_result = client
            .post(&resolve_url)
            .header("x-internal-key", &config.internal_api_key)
            .json(&serde_json::json!({
                "query": sub.intent_query,
                "user_wallet": sub.wallet_address,
            }))
            .send()
            .await;

        match resolve_result {
            Ok(resp) if resp.status().is_success() => {
                if sub.auto_execute {
                    info!(
                        "Subscription {sub_id}: auto_execute=true, would submit Blink transaction (placeholder)"
                    );
                } else {
                    info!(
                        "Subscription {sub_id}: auto_execute=false, notification would be sent"
                    );
                }

                // Update streak and next_due_at.
                let new_streak = sub.streak_current + 1;
                let new_longest = std::cmp::max(sub.streak_longest, new_streak);
                let new_multiplier = streak_multiplier(new_streak);

                // Convert frequency text to interval
                let interval = match sub.frequency.as_str() {
                    "daily" => "86400 seconds",
                    "weekly" => "604800 seconds",
                    "monthly" => "2592000 seconds",
                    _ => "86400 seconds",
                };

                sqlx::query(
                    r#"
                    UPDATE subscriptions
                    SET streak_current = $1,
                        streak_longest = $2,
                        multiplier = $3,
                        last_executed_at = NOW(),
                        next_due_at = NOW() + ($4)::interval,
                        updated_at = NOW()
                    WHERE subscription_id = $5
                    "#,
                )
                .bind(new_streak)
                .bind(new_longest)
                .bind(new_multiplier)
                .bind(interval)
                .bind(sub_id)
                .execute(pool)
                .await?;

                info!(
                    "Subscription {sub_id}: streak={new_streak}, multiplier={new_multiplier:.1}x"
                );
            }
            Ok(resp) => {
                warn!(
                    "Subscription {sub_id}: intent resolve returned status {}",
                    resp.status()
                );
                // Don't break streak on API failure — retry next poll.
            }
            Err(e) => {
                error!("Subscription {sub_id}: intent resolve failed: {e}");
                // Don't break streak on network failure.
            }
        }
    }

    Ok(())
}

/// Start the subscription poll cron loop.
pub fn start_subscription_cron(interval_secs: u64, pool: PgPool, config: Arc<Config>) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));

        // Skip the first immediate tick.
        interval.tick().await;

        loop {
            interval.tick().await;
            info!("Subscription poll cron tick");
            if let Err(e) = poll_subscriptions(&pool, &config).await {
                error!("Subscription poll error: {e}");
            }
        }
    });
}
