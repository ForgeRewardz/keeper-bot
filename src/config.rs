// ============================================================
// config.rs — Environment configuration
// ============================================================

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair};
use std::str::FromStr;

/// Shareable configuration (no Keypair — that lives in its own Arc).
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub solana_rpc_url: String,
    pub api_listen_addr: String,
    pub point_root_interval_secs: u64,
    pub rental_settle_interval_secs: u64,
    pub subscription_poll_interval_secs: u64,
    pub game_loop_interval_secs: u64,
    pub capacity_reset_interval_secs: u64,
    pub quality_score_interval_secs: u64,
    pub milestone_processor_interval_secs: u64,
    pub anti_abuse_interval_secs: u64,
    pub visibility_tick_interval_secs: u64,
    pub leaderboard_interval_secs: u64,
    pub stake_watcher_interval_secs: u64,
    pub rewardz_publish_interval_secs: u64,
    pub points_request_ttl_seconds: i64,
    pub points_receipt_ttl_seconds: i64,
    pub points_cors_origins: String,
    pub internal_api_key: String,
    pub api_base_url: String,
    pub program_id: Pubkey,
}

/// Load configuration and keypair from environment.
/// Panics on missing required vars or bad keypair file.
pub fn load() -> (Config, Keypair) {
    dotenvy::dotenv().ok();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");

    let keypair_path = std::env::var("KEYPAIR").expect("KEYPAIR is required");
    let keypair = read_keypair_file(&keypair_path)
        .unwrap_or_else(|e| panic!("Failed to read keypair from {keypair_path}: {e}"));

    let solana_rpc_url =
        std::env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8899".to_string());

    let api_listen_addr =
        std::env::var("API_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string());

    let point_root_interval_secs: u64 = std::env::var("POINT_ROOT_INTERVAL_SECS")
        .unwrap_or_else(|_| "28800".to_string())
        .parse()
        .expect("POINT_ROOT_INTERVAL_SECS must be a u64");

    let rental_settle_interval_secs: u64 = std::env::var("RENTAL_SETTLE_INTERVAL_SECS")
        .unwrap_or_else(|_| "604800".to_string())
        .parse()
        .expect("RENTAL_SETTLE_INTERVAL_SECS must be a u64");

    let subscription_poll_interval_secs: u64 = std::env::var("SUBSCRIPTION_POLL_INTERVAL_SECS")
        .unwrap_or_else(|_| "60".to_string())
        .parse()
        .expect("SUBSCRIPTION_POLL_INTERVAL_SECS must be a u64");

    let game_loop_interval_secs: u64 = std::env::var("GAME_LOOP_INTERVAL_SECS")
        .unwrap_or_else(|_| "15".to_string())
        .parse()
        .expect("GAME_LOOP_INTERVAL_SECS must be a u64");

    // League crons. Defaults chosen to match league-config.md cadence notes:
    //   - capacity-reset: hourly poll of an elapsed-window predicate (the
    //     actual weekly cadence is per-protocol via capacity_window_start).
    //   - quality-score:  hourly recompute per league-config §quality-score.
    //   - milestone-processor: every 5 minutes (task 15 spec).
    let capacity_reset_interval_secs: u64 = std::env::var("CAPACITY_RESET_INTERVAL_SECS")
        .unwrap_or_else(|_| "3600".to_string())
        .parse()
        .expect("CAPACITY_RESET_INTERVAL_SECS must be a u64");

    let quality_score_interval_secs: u64 = std::env::var("QUALITY_SCORE_INTERVAL_SECS")
        .unwrap_or_else(|_| "3600".to_string())
        .parse()
        .expect("QUALITY_SCORE_INTERVAL_SECS must be a u64");

    let milestone_processor_interval_secs: u64 = std::env::var("MILESTONE_PROCESSOR_INTERVAL_SECS")
        .unwrap_or_else(|_| "300".to_string())
        .parse()
        .expect("MILESTONE_PROCESSOR_INTERVAL_SECS must be a u64");

    // Anti-abuse scanner: hourly. Cheap aggregate queries; safe to
    // poll faster if we ever want quicker freeze response.
    let anti_abuse_interval_secs: u64 = std::env::var("ANTI_ABUSE_INTERVAL_SECS")
        .unwrap_or_else(|_| "3600".to_string())
        .parse()
        .expect("ANTI_ABUSE_INTERVAL_SECS must be a u64");

    // Visibility state machine: 15 minutes (task 14 spec).
    let visibility_tick_interval_secs: u64 = std::env::var("VISIBILITY_TICK_INTERVAL_SECS")
        .unwrap_or_else(|_| "900".to_string())
        .parse()
        .expect("VISIBILITY_TICK_INTERVAL_SECS must be a u64");

    // Leaderboard snapshot: daily (task 17 spec). Snapshot is keyed
    // on CURRENT_DATE so the cadence can be shorter for testing
    // without producing duplicate rows.
    let leaderboard_interval_secs: u64 = std::env::var("LEADERBOARD_INTERVAL_SECS")
        .unwrap_or_else(|_| "86400".to_string())
        .parse()
        .expect("LEADERBOARD_INTERVAL_SECS must be a u64");

    // Stake watcher (task 16a): polls on-chain ProtocolStake PDAs and
    // mirrors active_stake into the DB. 60s default — unlock needs to
    // show up inside a console walkthrough but doesn't require sub-slot
    // latency. Override via STAKE_WATCHER_INTERVAL_SECS for rehearsals.
    let stake_watcher_interval_secs: u64 = std::env::var("STAKE_WATCHER_INTERVAL_SECS")
        .unwrap_or_else(|_| "60".to_string())
        .parse()
        .expect("STAKE_WATCHER_INTERVAL_SECS must be a u64");

    // Rewardz publisher cron (task 33). Default matches
    // LeagueConfig.rewardz_publish_interval_secs (3600s devnet); override
    // via REWARDZ_PUBLISH_INTERVAL_SECS for rehearsal or tests.
    let rewardz_publish_interval_secs: u64 = std::env::var("REWARDZ_PUBLISH_INTERVAL_SECS")
        .unwrap_or_else(|_| "3600".to_string())
        .parse()
        .expect("REWARDZ_PUBLISH_INTERVAL_SECS must be a u64");

    let points_request_ttl_seconds: i64 = std::env::var("POINTS_REQUEST_TTL_SECONDS")
        .unwrap_or_else(|_| "300".to_string())
        .parse()
        .expect("POINTS_REQUEST_TTL_SECONDS must be i64");

    let points_receipt_ttl_seconds: i64 = std::env::var("POINTS_RECEIPT_TTL_SECONDS")
        .unwrap_or_else(|_| "600".to_string())
        .parse()
        .expect("POINTS_RECEIPT_TTL_SECONDS must be i64");

    let points_cors_origins =
        std::env::var("POINTS_CORS_ORIGINS").unwrap_or_else(|_| "*".to_string());

    let internal_api_key = std::env::var("INTERNAL_API_KEY").expect("INTERNAL_API_KEY is required");

    let api_base_url =
        std::env::var("API_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:3001".to_string());

    let program_id_str = std::env::var("PROGRAM_ID").expect("PROGRAM_ID is required");
    let program_id =
        Pubkey::from_str(&program_id_str).unwrap_or_else(|e| panic!("Invalid PROGRAM_ID: {e}"));

    let config = Config {
        database_url,
        solana_rpc_url,
        api_listen_addr,
        point_root_interval_secs,
        rental_settle_interval_secs,
        subscription_poll_interval_secs,
        game_loop_interval_secs,
        capacity_reset_interval_secs,
        quality_score_interval_secs,
        milestone_processor_interval_secs,
        anti_abuse_interval_secs,
        visibility_tick_interval_secs,
        leaderboard_interval_secs,
        stake_watcher_interval_secs,
        rewardz_publish_interval_secs,
        points_request_ttl_seconds,
        points_receipt_ttl_seconds,
        points_cors_origins,
        internal_api_key,
        api_base_url,
        program_id,
    };

    (config, keypair)
}
