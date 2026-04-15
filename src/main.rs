// ============================================================
// main.rs — CLI dispatcher for mvp-keeper-bot
// ============================================================

mod api;
mod config;
mod db;
mod game_loop;
mod merkle;
mod publisher;
mod rental;
mod subscriptions;

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
}

#[tokio::main]
async fn main() {
    // Initialize tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Full);

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
}
