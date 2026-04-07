// ============================================================
// burn_watcher.rs — Burn-to-mint point deduction watcher
// ============================================================

use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use sqlx::postgres::PgPool;
use std::time::Duration;
use tokio::time;
use tracing::{error, info, warn};

use crate::db;

/// On-chain discriminator for MintAttempt accounts.
const DISC_MINT_ATTEMPT: u8 = 6;

/// MintAttempt account layout offsets (2-byte prefix + data):
/// | Offset | Size | Field          |
/// | 0      | 32   | authority      |
/// | 32     | 8    | points_burned  |
/// (additional fields exist but we only need these two)
const OFF_AUTHORITY: usize = 2; // after 2-byte prefix
const OFF_POINTS_BURNED: usize = 2 + 32; // after prefix + authority

/// Watch for MintAttempt accounts and deduct points from the DB.
pub async fn watch_mint_attempts(
    rpc: &RpcClient,
    pool: &PgPool,
    program_id: &Pubkey,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Scanning for MintAttempt accounts");

    // Fetch all MintAttempt accounts via getProgramAccounts with discriminator filter.
    let accounts = rpc.get_program_accounts_with_config(
        program_id,
        solana_client::rpc_config::RpcProgramAccountsConfig {
            filters: Some(vec![solana_client::rpc_filter::RpcFilterType::Memcmp(
                solana_client::rpc_filter::Memcmp::new_raw_bytes(
                    0,
                    vec![DISC_MINT_ATTEMPT],
                ),
            )]),
            account_config: Default::default(),
            with_context: None,
            sort_results: None,
        },
    )?;

    if accounts.is_empty() {
        info!("No MintAttempt accounts found");
        return Ok(());
    }

    info!("Found {} MintAttempt account(s)", accounts.len());

    for (pda, account) in &accounts {
        let data = &account.data;

        // Need at least prefix + authority(32) + points_burned(8) = 42 bytes.
        if data.len() < 42 {
            warn!("MintAttempt {pda}: data too short ({} bytes), skipping", data.len());
            continue;
        }

        // Read authority (32 bytes at offset 2).
        let authority_bytes: [u8; 32] = data[OFF_AUTHORITY..OFF_AUTHORITY + 32]
            .try_into()
            .unwrap();
        let authority = Pubkey::from(authority_bytes);

        // Read points_burned (u64 LE at offset 34).
        let points_burned = u64::from_le_bytes(
            data[OFF_POINTS_BURNED..OFF_POINTS_BURNED + 8]
                .try_into()
                .unwrap(),
        );

        if points_burned == 0 {
            continue;
        }

        let wallet = authority.to_string();
        let mint_attempt_pda = pda.to_string();

        match db::deduct_user_points(pool, &wallet, points_burned as i64, &mint_attempt_pda).await
        {
            Ok(true) => {
                info!(
                    "Deducted {points_burned} points from {wallet} (MintAttempt: {mint_attempt_pda})"
                );
            }
            Ok(false) => {
                // Already processed — idempotent, no action needed.
            }
            Err(e) => {
                error!(
                    "Failed to deduct points for {wallet} (MintAttempt: {mint_attempt_pda}): {e}"
                );
            }
        }
    }

    Ok(())
}

/// Start the burn watcher cron loop.
/// Polls aggressively because MintAttempt accounts are ephemeral.
pub fn start_burn_watcher_cron(
    interval_secs: u64,
    pool: PgPool,
    rpc_url: String,
    program_id: Pubkey,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        let rpc = RpcClient::new(rpc_url);

        // Skip the first immediate tick.
        interval.tick().await;

        loop {
            interval.tick().await;
            if let Err(e) = watch_mint_attempts(&rpc, &pool, &program_id).await {
                error!("Burn watcher error: {e}");
            }
        }
    });
}
