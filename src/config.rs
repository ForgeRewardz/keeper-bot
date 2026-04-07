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
        points_request_ttl_seconds,
        points_receipt_ttl_seconds,
        points_cors_origins,
        internal_api_key,
        api_base_url,
        program_id,
    };

    (config, keypair)
}
