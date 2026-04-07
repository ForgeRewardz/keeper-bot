// ============================================================
// publisher.rs — Merkle root publisher cron
// ============================================================

use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};
use sqlx::postgres::PgPool;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{error, info, warn};

use crate::db;
use crate::merkle;

/// PDA seed for the PointRoot account (must match on-chain).
const POINT_ROOT_SEED: &[u8] = b"point_root";

/// PDA seed for the Config account (must match on-chain).
const CONFIG_SEED: &[u8] = b"config";

/// Instruction discriminator for set_point_root.
/// This is determined by the on-chain program's instruction dispatch.
/// Update this value to match the actual discriminator in the deployed program.
const SET_POINT_ROOT_DISCRIMINATOR: u8 = 7;

/// Publish the Merkle root on-chain.
pub async fn publish_root(
    pool: &PgPool,
    rpc: &RpcClient,
    keypair: &Keypair,
    program_id: &Pubkey,
    epoch: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Read all users with points > 0.
    let users = db::get_users_with_points(pool).await?;
    if users.is_empty() {
        info!("No users with points > 0, skipping root publish");
        return Ok(());
    }

    // 2. Build entries for the Merkle tree.
    let entries: Vec<(Pubkey, u64)> = users
        .iter()
        .filter_map(|u| {
            let pk = Pubkey::from_str(&u.wallet_address).ok()?;
            if u.total_points <= 0 {
                return None;
            }
            Some((pk, u.total_points as u64))
        })
        .collect();

    if entries.is_empty() {
        info!("No valid entries after filtering, skipping root publish");
        return Ok(());
    }

    // 3. Build Merkle tree.
    let tree = merkle::build_merkle_tree(&entries)
        .ok_or("Failed to build Merkle tree (no entries)")?;

    info!(
        "Built Merkle tree: root={}, entries={}, epoch={}",
        bs58::encode(&tree.root).into_string(),
        entries.len(),
        epoch,
    );

    // 4. Derive PDAs.
    let (point_root_pda, _) = Pubkey::find_program_address(&[POINT_ROOT_SEED], program_id);
    let (config_pda, _) = Pubkey::find_program_address(&[CONFIG_SEED], program_id);

    // 5. Build instruction data: discriminator(1) + root(32) + epoch(8) + total_issued(16) = 57 bytes.
    let total_issued: u128 = entries.iter().map(|(_, pts)| *pts as u128).sum();
    let mut ix_data = Vec::with_capacity(57);
    ix_data.push(SET_POINT_ROOT_DISCRIMINATOR);
    ix_data.extend_from_slice(&tree.root);
    ix_data.extend_from_slice(&epoch.to_le_bytes());
    ix_data.extend_from_slice(&total_issued.to_le_bytes());

    // 6. Build instruction.
    let ix = Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(keypair.pubkey(), true),   // ops_authority (signer, writable for fees)
            AccountMeta::new_readonly(config_pda, false), // config
            AccountMeta::new(point_root_pda, false),     // point_root (writable)
            #[allow(deprecated)]
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false), // system_program
        ],
        data: ix_data,
    };

    // 7. Submit transaction.
    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&keypair.pubkey()),
        &[keypair],
        recent_blockhash,
    );

    match rpc.send_and_confirm_transaction(&tx) {
        Ok(sig) => {
            info!("Published root on-chain, tx={sig}");
        }
        Err(e) => {
            error!("Failed to publish root: {e}");
            return Err(e.into());
        }
    }

    // 8. Save proofs to DB.
    let proofs_for_db: Vec<(String, i64, Vec<Vec<u8>>)> = entries
        .iter()
        .filter_map(|(pk, pts)| {
            let proof_hashes = tree.proofs.get(pk)?;
            let proof_vecs: Vec<Vec<u8>> = proof_hashes.iter().map(|h| h.to_vec()).collect();
            Some((pk.to_string(), *pts as i64, proof_vecs))
        })
        .collect();

    if let Err(e) = db::save_merkle_proofs(pool, epoch as i64, &tree.root, &proofs_for_db).await {
        warn!("Failed to save proofs to DB (root already on-chain): {e}");
    }

    info!("Root publish complete for epoch {epoch}");
    Ok(())
}

/// Start the publisher cron loop.
pub fn start_publisher_cron(
    interval_secs: u64,
    pool: PgPool,
    rpc_url: String,
    keypair: Arc<Keypair>,
    program_id: Pubkey,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        let rpc = RpcClient::new(rpc_url);

        // Load current epoch from on-chain PointRoot to survive process restarts.
        let mut epoch: u64 = match load_current_epoch(&rpc, &program_id) {
            Some(e) => {
                info!("Loaded on-chain epoch: {e}");
                e
            }
            None => {
                info!("No on-chain epoch found, starting from 0");
                0
            }
        };

        // Skip the first immediate tick, then run on schedule.
        interval.tick().await;

        loop {
            interval.tick().await;
            epoch += 1;

            info!("Publisher cron tick: epoch={epoch}");
            if let Err(e) = publish_root(&pool, &rpc, &keypair, &program_id, epoch).await {
                error!("Publisher cron error: {e}");
            }
        }
    });
}

/// Read the current epoch from the on-chain PointRoot account.
fn load_current_epoch(rpc: &RpcClient, program_id: &Pubkey) -> Option<u64> {
    let (point_root_pda, _) = Pubkey::find_program_address(&[POINT_ROOT_SEED], program_id);
    match rpc.get_account(&point_root_pda) {
        Ok(account) => {
            let data = &account.data;
            // PointRoot layout: 2-byte prefix + 32 authority + 32 root + 8 epoch
            if data.len() >= 74 {
                let epoch_bytes: [u8; 8] = data[66..74].try_into().ok()?;
                Some(u64::from_le_bytes(epoch_bytes))
            } else {
                None
            }
        }
        Err(e) => {
            warn!("Could not load on-chain epoch: {e}");
            None
        }
    }
}
