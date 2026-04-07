// ============================================================
// rental.rs — Rental settlement cron
// ============================================================

use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    account::Account,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};
use sqlx::postgres::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{error, info, warn};

/// On-chain discriminator for RentalAgreement accounts.
const DISC_RENTAL_AGREEMENT: u8 = 4;

/// Instruction discriminator for settle_rental.
/// Update this value to match the deployed program.
const SETTLE_RENTAL_DISCRIMINATOR: u8 = 12;

/// Settle all active rental agreements.
pub async fn settle_rentals(
    rpc: &RpcClient,
    keypair: &Keypair,
    _pool: &PgPool,
    program_id: &Pubkey,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Starting rental settlement scan");

    // 1. Fetch all RentalAgreement PDAs via getProgramAccounts with discriminator filter.
    let accounts = rpc.get_program_accounts_with_config(
        program_id,
        solana_client::rpc_config::RpcProgramAccountsConfig {
            filters: Some(vec![solana_client::rpc_filter::RpcFilterType::Memcmp(
                solana_client::rpc_filter::Memcmp::new_raw_bytes(
                    0, // discriminator at offset 0
                    vec![DISC_RENTAL_AGREEMENT],
                ),
            )]),
            account_config: Default::default(),
            with_context: None,
            sort_results: None,
        },
    )?;

    if accounts.is_empty() {
        info!("No active rental agreements found");
        return Ok(());
    }

    info!("Found {} rental agreement(s) to settle", accounts.len());

    let mut success_count = 0u32;
    let mut fail_count = 0u32;

    for (rental_pda, account) in &accounts {
        match settle_single_rental(rpc, keypair, program_id, rental_pda, account) {
            Ok(()) => {
                info!("Settled rental: {rental_pda}");
                success_count += 1;
            }
            Err(e) => {
                warn!("Failed to settle rental {rental_pda}: {e}");
                fail_count += 1;
            }
        }
    }

    info!("Rental settlement complete: {success_count} success, {fail_count} failed");
    Ok(())
}

fn settle_single_rental(
    rpc: &RpcClient,
    keypair: &Keypair,
    program_id: &Pubkey,
    rental_pda: &Pubkey,
    _account: &Account,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Build settle_rental instruction.
    // Accounts layout (simplified — adjust per actual program):
    //   0. ops_authority (signer)
    //   1. rental_agreement (writable)
    //   2. system_program
    let ix = Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(keypair.pubkey(), true),
            AccountMeta::new(*rental_pda, false),
            #[allow(deprecated)]
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        ],
        data: vec![SETTLE_RENTAL_DISCRIMINATOR],
    };

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&keypair.pubkey()),
        &[keypair],
        recent_blockhash,
    );

    rpc.send_and_confirm_transaction(&tx)?;
    Ok(())
}

/// Start the rental settlement cron loop.
pub fn start_rental_cron(
    interval_secs: u64,
    pool: PgPool,
    rpc_url: String,
    keypair: Arc<Keypair>,
    program_id: Pubkey,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        let rpc = RpcClient::new(rpc_url);

        // Skip the first immediate tick.
        interval.tick().await;

        loop {
            interval.tick().await;
            info!("Rental settlement cron tick");
            if let Err(e) = settle_rentals(&rpc, &keypair, &pool, &program_id).await {
                error!("Rental settlement error: {e}");
            }
        }
    });
}
