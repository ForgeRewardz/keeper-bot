// ============================================================
// main.rs — CLI dispatcher for mvp-keeper-bot
// ============================================================

mod anti_abuse;
mod api;
mod capacity_reset;
mod config;
mod db;
mod game_loop;
mod hex;
mod leaderboard;
mod league_config;
mod merkle;
mod milestone_processor;
mod publisher;
mod quality_score;
mod rental;
mod rewardz_merkle;
mod rewardz_publisher;
mod stake_watcher;
mod subscriptions;
mod visibility_tick;

use clap::{Parser, Subcommand};
use std::sync::Arc;
use tracing::info;

#[derive(Parser)]
#[command(name = "mvp-keeper-bot")]
#[command(about = "REWARDZ keeper bot — Merkle root publisher, receipt signer, settlement engine")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP API server only
    ServeApi,
    /// Run cron jobs only (publisher, rental, subscriptions, mining game loop)
    RunCrons,
    /// Run both API server and cron jobs (default)
    Full,
    /// Print the resolved LeagueConfig as JSON to stdout and exit.
    /// Used by the TS ↔ Rust parity test. Respects SOLANA_NETWORK.
    PrintLeagueConfig,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Full);

    // Pure subcommands that must keep stdout clean (the parity test parses it as JSON).
    // Skip tracing init and all config/DB work here.
    if let Command::PrintLeagueConfig = command {
        println!("{}", league_config::dump_json());
        return;
    }

    // Initialize tracing for long-running modes only.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load configuration.
    let (config, keypair) = config::load();
    info!("Loaded configuration, program_id={}", config.program_id);

    // Create DB pool.
    let pool = db::create_pool(&config.database_url)
        .await
        .expect("Failed to create database pool");
    info!("Connected to database");

    let keypair = Arc::new(keypair);
    let config = Arc::new(config);

    match command {
        Command::ServeApi => {
            info!("Starting API server");
            run_api(&config, &pool, keypair.clone()).await;
        }
        Command::RunCrons => {
            info!("Starting cron jobs");
            run_crons(&config, &pool, keypair.clone());
            // Keep the process alive until ctrl-c.
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to listen for ctrl-c");
            info!("Shutting down crons");
        }
        Command::Full => {
            info!("Starting full mode (API + crons)");
            run_crons(&config, &pool, keypair.clone());
            run_api(&config, &pool, keypair.clone()).await;
        }
        Command::PrintLeagueConfig => unreachable!("handled before config load"),
    }
}

async fn run_api(
    config: &Arc<config::Config>,
    pool: &sqlx::PgPool,
    keypair: Arc<solana_sdk::signature::Keypair>,
) {
    let state = api::AppState {
        pool: pool.clone(),
        rpc_url: config.solana_rpc_url.clone(),
        keypair,
        program_id: config.program_id,
        points_request_ttl_seconds: config.points_request_ttl_seconds,
        points_receipt_ttl_seconds: config.points_receipt_ttl_seconds,
        rewardz_publish_interval_secs: config.rewardz_publish_interval_secs as i64,
    };

    let app = api::create_router(state, &config.points_cors_origins);
    let listener = tokio::net::TcpListener::bind(&config.api_listen_addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind to {}: {e}", config.api_listen_addr));

    info!("API server listening on {}", config.api_listen_addr);
    axum::serve(listener, app).await.expect("API server error");
}

fn run_crons(
    config: &Arc<config::Config>,
    pool: &sqlx::PgPool,
    keypair: Arc<solana_sdk::signature::Keypair>,
) {
    // Publisher cron.
    publisher::start_publisher_cron(
        config.point_root_interval_secs,
        pool.clone(),
        config.solana_rpc_url.clone(),
        keypair.clone(),
        config.program_id,
    );

    // Rental settlement cron.
    rental::start_rental_cron(
        config.rental_settle_interval_secs,
        pool.clone(),
        config.solana_rpc_url.clone(),
        keypair.clone(),
        config.program_id,
    );

    // Subscription executor cron.
    subscriptions::start_subscription_cron(
        config.subscription_poll_interval_secs,
        pool.clone(),
        config.clone(),
    );

    // Mining game loop cron.
    game_loop::start_game_loop_cron(
        config.game_loop_interval_secs,
        config.solana_rpc_url.clone(),
        keypair.clone(),
        config.program_id,
    );

    // Optional per-player checkpoint cranker (off by default).
    // Set KEEPER_RUN_CRANKER=true to enable.
    game_loop::start_cranker_loop(
        config.game_loop_interval_secs,
        config.solana_rpc_url.clone(),
        keypair.clone(),
        config.program_id,
    );

    // Colosseum Rewardz League crons (tasks 11 / 13 / 15).
    // Each starter spawns its own tokio task; no shared state beyond the
    // PgPool handle.
    capacity_reset::start_capacity_reset_cron(
        config.capacity_reset_interval_secs,
        pool.clone(),
    );
    quality_score::start_quality_score_cron(
        config.quality_score_interval_secs,
        pool.clone(),
    );
    milestone_processor::start_milestone_processor_cron(
        config.milestone_processor_interval_secs,
        pool.clone(),
    );

    // Anti-abuse scanner (task 16) — feeds abuse_flags consumed by
    // milestone-processor and visibility-tick.
    anti_abuse::start_anti_abuse_cron(
        config.anti_abuse_interval_secs,
        pool.clone(),
    );

    // Visibility state machine (task 14) — depends on capacity,
    // activity, and abuse_flags.
    visibility_tick::start_visibility_tick_cron(
        config.visibility_tick_interval_secs,
        pool.clone(),
    );

    // Daily leaderboard snapshot + bonus issuance (task 17).
    leaderboard::start_leaderboard_cron(
        config.leaderboard_interval_secs,
        pool.clone(),
    );

    // Stake-to-unlock-capacity watcher (task 16a) — mirrors on-chain
    // ProtocolStake into protocols.active_stake and resets
    // remaining_capacity = issuance_ratio × active_stake on the first
    // observed stake transition.
    stake_watcher::start_stake_watcher_cron(
        config.stake_watcher_interval_secs,
        pool.clone(),
        config.solana_rpc_url.clone(),
        config.program_id,
    );

    // Rewardz Merkle root publisher (task 33) — sibling of
    // `publisher::start_publisher_cron` but for the SPL-token settlement
    // rail. Retry-same-epoch on failure per design §Q1.
    rewardz_publisher::start_rewardz_publisher_cron(
        config.rewardz_publish_interval_secs,
        pool.clone(),
        config.solana_rpc_url.clone(),
        keypair.clone(),
        config.program_id,
    );
}
