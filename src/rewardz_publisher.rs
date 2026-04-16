// ============================================================
// rewardz_publisher.rs — Merkle root publisher for the Rewardz
// SPL token settlement rail (TODO-0018 task 33).
//
// Sibling of `publisher.rs` (PointRoot publisher) rather than a
// parameterised helper, because the two rails diverge on: leaf
// semantics (cumulative vs per-epoch), on-chain IX layout (deny-list,
// u128 total), and retry discipline (rewardz retries the SAME epoch
// on failure per design note §Q1 — points does not).
//
// Source of truth for the cron's decisions:
//   - rewardz-claim-design.md §Keeper cron (steps 1–6)
//   - rewardz-claim-design.md §Q1 (monotonic-retry-same-epoch)
//   - rewardz-claim-design.md §Q3 (empty-root liveness)
//
// CRITICAL: instruction data layout MUST match the on-chain parser at
// `mvp-smart-contracts/program/src/rewardz.rs` exactly. See the
// `HEADER_LEN` + per-entry stride there.
// ============================================================

use rewardz_mvp_api::state::{
    rroot_read_deny_entry, rroot_read_deny_list_len, rroot_read_root, rroot_read_u64, RewardzRoot,
    REWARDZ_DENY_LIST_MAX,
};
use rewardz_mvp_api::{CONFIG_SEED, IX_SET_REWARDZ_ROOT, REWARDZ_ROOT_SEED};
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
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info, warn};

use crate::db;
use crate::league_config::load_league_config;
use crate::rewardz_merkle::build_rewardz_tree;

/// IX_SET_REWARDZ_ROOT instruction data header size:
/// disc(1) + root(32) + epoch(8) + total_rewardz_issued(16) + deny_list_len(1) = 58.
const IX_HEADER_LEN: usize = 58;

/// When `REWARDZ_PUBLISHER_DRY_RUN=1` (or `=true`) is set, the cron
/// builds the Merkle tree and logs it, but does NOT submit an on-chain
/// tx. Mandatory for the first devnet publish per design note §Security.
fn is_dry_run() -> bool {
    matches!(
        std::env::var("REWARDZ_PUBLISHER_DRY_RUN").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Publish one Rewardz Merkle root on-chain. Idempotent against a given
/// `epoch` — on failure the caller should retry with the **same** epoch
/// number (design note §Q1).
pub async fn publish_rewardz_root(
    pool: &PgPool,
    rpc: &RpcClient,
    keypair: &Keypair,
    program_id: &Pubkey,
    epoch: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cfg = load_league_config();

    // Capture a cutoff from the DB clock BEFORE reading earnings so
    // the Step 5 backfill UPDATE can ignore rows INSERTed after the
    // tree was built. Using DB time (not wall-clock) eliminates
    // clock-skew between keeper and Postgres as a concern.
    let cutoff: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT NOW()").fetch_one(pool).await?;

    // Step 1 (design §Keeper cron): SUM earnings by authority.
    let rows = db::get_rewardz_earnings_grouped(pool).await?;

    // Step 3 (computed first to drive the cap check): build the list
    // of tree entries. `get_rewardz_earnings_grouped` already sorts by
    // authority so the root is deterministic. Rows with a malformed
    // protocol_authority or non-positive amount are dropped — they
    // cannot produce a claimable leaf.
    let entries: Vec<(Pubkey, u64)> = rows
        .iter()
        .filter_map(|r| {
            let pk = Pubkey::from_str(&r.protocol_authority).ok()?;
            if r.cumulative_amount <= 0 {
                return None;
            }
            Some((pk, r.cumulative_amount as u64))
        })
        .collect();
    let dropped = rows.len() - entries.len();
    if dropped > 0 {
        warn!(
            "[rewardz-publisher] dropped {dropped} unredeemable earnings row(s) \
             (bad pubkey or amount <= 0) — excluded from total_rewardz_issued"
        );
    }

    // Step 2: enforce per-epoch cap. Computed over `entries` (post-
    // filter) so `total_rewardz_issued` matches the sum of claimable
    // leaves — mirrors the PointRoot publisher's invariant and avoids
    // phantom on-chain issuance that no proof could redeem. We refuse
    // to publish a root that exceeds the cap rather than "split into
    // multiple epochs" (partial-tree logic not yet designed). An
    // operator seeing this error should either lower pending earnings
    // or raise the LeagueConfig cap via governance.
    let total: u128 = entries.iter().map(|(_, amt)| *amt as u128).sum();
    if total > cfg.max_rewardz_per_epoch as u128 {
        error!(
            "[rewardz-publisher] cumulative total {total} exceeds \
             max_rewardz_per_epoch {cap} — refusing to publish epoch {epoch}",
            cap = cfg.max_rewardz_per_epoch,
        );
        return Err(format!(
            "cumulative rewardz total {total} exceeds per-epoch cap {}",
            cfg.max_rewardz_per_epoch
        )
        .into());
    }

    let root: [u8; 32] = match build_rewardz_tree(&entries) {
        Some(t) => t.root,
        None => {
            // §Q3: publish empty root for liveness. Zero-byte root
            // signals "keeper healthy, no earnings yet"; epoch still
            // advances so observers can distinguish liveness from
            // staleness.
            info!(
                "[rewardz-publisher] no earnings — publishing zero-root \
                 for liveness (epoch={epoch})"
            );
            [0u8; 32]
        }
    };

    info!(
        "[rewardz-publisher] tree built: root={root_b58}, entries={n}, total={total}, epoch={epoch}",
        root_b58 = bs58::encode(&root).into_string(),
        n = entries.len(),
    );

    // Step 6 (moved up): dry-run short-circuit. Must gate BEFORE we
    // touch RPC or build an IX — the whole point of dry-run is to
    // validate the tree without any on-chain mutation.
    if is_dry_run() {
        info!(
            "[rewardz-publisher] REWARDZ_PUBLISHER_DRY_RUN=1 — skipping \
             tx submission (epoch={epoch})"
        );
        return Ok(());
    }

    // Step 4: submit `set_rewardz_root` tx.
    //
    // `set_rewardz_root` overwrites the deny_list wholesale on every
    // call (see program/src/rewardz.rs line ~168). Passing an empty
    // deny_list here would silently clear any ops-applied freezes, so
    // we read the current on-chain live-prefix and echo it unchanged.
    // A separate ops flow (out of scope for task 33) is responsible
    // for adding/removing entries.
    let (rewardz_root_pda, _) = Pubkey::find_program_address(&[REWARDZ_ROOT_SEED], program_id);
    let (config_pda, _) = Pubkey::find_program_address(&[CONFIG_SEED], program_id);
    // Propagate RPC errors here rather than defaulting to an empty
    // deny_list — a transient read failure against an existing
    // RewardzRoot would otherwise silently wipe the live freeze list.
    let deny_list = read_on_chain_deny_list(rpc, &rewardz_root_pda)?;

    let mut ix_data = Vec::with_capacity(IX_HEADER_LEN + deny_list.len() * 32);
    ix_data.push(IX_SET_REWARDZ_ROOT);
    ix_data.extend_from_slice(&root);
    ix_data.extend_from_slice(&epoch.to_le_bytes());
    ix_data.extend_from_slice(&total.to_le_bytes());
    ix_data.push(deny_list.len() as u8);
    for entry in &deny_list {
        ix_data.extend_from_slice(entry);
    }

    let ix = Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(keypair.pubkey(), true),
            AccountMeta::new_readonly(config_pda, false),
            AccountMeta::new(rewardz_root_pda, false),
            #[allow(deprecated)]
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        ],
        data: ix_data,
    };

    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&keypair.pubkey()),
        &[keypair],
        recent_blockhash,
    );

    let sig = match rpc.send_and_confirm_transaction(&tx) {
        Ok(sig) => {
            info!(
                "[rewardz-publisher] published rewardz root epoch={epoch}, tx={sig}"
            );
            sig
        }
        Err(e) => {
            error!("[rewardz-publisher] failed to publish epoch {epoch}: {e}");
            return Err(e.into());
        }
    };

    // Step 5 (design §Keeper cron): on confirmation, record the
    // rewardz_root_epochs row and backfill included_in_root_epoch on
    // each earnings row that fed the tree. The authorities list is
    // the exact set filtered into `entries` above — rows whose
    // authority was dropped (malformed pubkey or non-positive
    // cumulative) stay unmarked and roll forward into a future epoch
    // once their cumulative crosses zero.
    //
    // We log-but-don't-propagate errors from this step: the on-chain
    // tx is already confirmed and the claim rail is live, so
    // returning Err would trigger the §Q1 retry-same-epoch logic —
    // but the chain has already advanced past `epoch` and the next
    // tick's `on-chain + 1` is what we want. A DB write failure here
    // is an observability gap, not a claim-rail outage.
    let authorities: Vec<String> = entries.iter().map(|(pk, _)| pk.to_string()).collect();
    // `epoch as i64` is safe: epoch is a monotonic per-tick counter
    // (RewardzRoot.epoch) that starts at 1 and advances by 1 per
    // publisher tick. At one tick per minute it would take ~17.5
    // trillion years to overflow i64::MAX, well past the heat death
    // concern window. Postgres `rewardz_root_epochs.epoch` is BIGINT
    // (signed i64) so narrowing here matches the column type.
    match db::record_rewardz_root_publish(
        pool,
        epoch as i64,
        &root,
        &sig.to_string(),
        total,
        &authorities,
        cutoff,
    )
    .await
    {
        Ok(backfilled) => info!(
            "[rewardz-publisher] recorded epoch={epoch} + backfilled {backfilled} earnings row(s)"
        ),
        Err(e) => error!(
            "[rewardz-publisher] epoch={epoch} tx confirmed ({sig}) but DB write failed: \
             {e} — observability gap for this epoch, claim rail unaffected"
        ),
    }

    Ok(())
}

/// Start the cron loop. Each tick:
///   1. read on-chain epoch (authoritative source).
///   2. publish epoch = on-chain + 1.
///
/// On failure the in-memory state does NOT advance — the next tick re-
/// reads the same on-chain epoch and retries the same number. This
/// upholds the monotonic-no-holes invariant from design note §Q1.
pub fn start_rewardz_publisher_cron(
    interval_secs: u64,
    pool: PgPool,
    rpc_url: String,
    keypair: Arc<Keypair>,
    program_id: Pubkey,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        // Burst-catch-up after a slow tick is never what we want for a
        // chain writer — one delayed tick is fine, N stacked ones would
        // spam the network.
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let rpc = RpcClient::new(rpc_url);

        // Skip the first immediate tick — let the rest of the system warm up.
        interval.tick().await;

        // Cached high-water mark of the on-chain epoch. Lets us detect
        // transient RPC read failures AFTER initialisation (Ok(None))
        // and skip the tick rather than misinterpreting them as
        // "account missing → publish epoch 1" (which the chain would
        // reject but with a misleading log trail).
        let mut last_known_epoch: u64 = 0;

        loop {
            interval.tick().await;

            let current = match load_current_epoch(&rpc, &program_id) {
                Ok(Some(e)) => {
                    last_known_epoch = e;
                    e
                }
                Ok(None) => {
                    // Account truly not initialised yet. If we previously
                    // saw a non-zero epoch, something is very wrong
                    // (account was closed?) — skip rather than retry
                    // from 1 and clobber the chain.
                    if last_known_epoch > 0 {
                        warn!(
                            "[rewardz-publisher] on-chain RewardzRoot unexpectedly \
                             missing (last seen epoch={last_known_epoch}) — \
                             skipping tick"
                        );
                        continue;
                    }
                    0
                }
                Err(e) => {
                    // Transient RPC error. Never publish from stale
                    // state — next tick will retry the read.
                    error!(
                        "[rewardz-publisher] RPC error reading on-chain epoch \
                         (last seen={last_known_epoch}): {e} — skipping tick"
                    );
                    continue;
                }
            };

            let next_epoch = current + 1;
            info!(
                "[rewardz-publisher] tick: on-chain epoch={current}, \
                 publishing epoch={next_epoch}"
            );

            if let Err(e) =
                publish_rewardz_root(&pool, &rpc, &keypair, &program_id, next_epoch).await
            {
                error!(
                    "[rewardz-publisher] error (will retry epoch {next_epoch} \
                     on next tick): {e}"
                );
            }
        }
    });
}

/// Read the current on-chain epoch from the RewardzRoot PDA. The
/// return shape distinguishes three cases that the cron must handle
/// differently:
///   - `Ok(Some(epoch))` — account exists and is well-formed.
///   - `Ok(None)` — account not yet initialised (first publish).
///   - `Err(e)` — transient RPC failure; caller must NOT treat this
///     as "account missing" (doing so would publish epoch 1 against
///     a live chain and produce a misleading error trail).
fn load_current_epoch(
    rpc: &RpcClient,
    program_id: &Pubkey,
) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
    let (pda, _) = Pubkey::find_program_address(&[REWARDZ_ROOT_SEED], program_id);
    let response = rpc.get_account_with_commitment(&pda, rpc.commitment())?;
    let acct = match response.value {
        Some(a) => a,
        None => return Ok(None),
    };
    if acct.data.len() < RewardzRoot::LEN {
        warn!(
            "[rewardz-publisher] RewardzRoot account too small ({} bytes)",
            acct.data.len()
        );
        return Ok(None);
    }
    Ok(Some(rroot_read_u64(&acct.data, RewardzRoot::OFF_EPOCH)))
}

/// Read the on-chain (root, epoch) from the RewardzRoot PDA. Returns
/// `Ok(None)` when the account is not yet initialised (first publish
/// has not happened) and `Err` on transient RPC failure. Callers (the
/// `GET /rewardz/proof/:authority` endpoint) need both pieces in one
/// read because the response embeds the on-chain epoch for the claim
/// IX to validate against. Distinct from `load_current_epoch` which
/// returns only the epoch (for the cron's next-epoch calculation) and
/// is kept private to this module — exposing a `(root, epoch)` helper
/// lets api.rs avoid duplicating the PDA + offset arithmetic.
pub fn read_onchain_rewardz_root(
    rpc: &RpcClient,
    program_id: &Pubkey,
) -> Result<Option<([u8; 32], u64)>, Box<dyn std::error::Error + Send + Sync>> {
    let (pda, _) = Pubkey::find_program_address(&[REWARDZ_ROOT_SEED], program_id);
    let response = rpc.get_account_with_commitment(&pda, rpc.commitment())?;
    let acct = match response.value {
        Some(a) => a,
        None => return Ok(None),
    };
    if acct.data.len() < RewardzRoot::LEN {
        warn!(
            "[rewardz-publisher] RewardzRoot account too small ({} bytes)",
            acct.data.len()
        );
        return Ok(None);
    }
    let root = *rroot_read_root(&acct.data);
    let epoch = rroot_read_u64(&acct.data, RewardzRoot::OFF_EPOCH);
    Ok(Some((root, epoch)))
}

/// Read the live prefix of the on-chain deny_list. Distinguishes
/// "account not yet initialised" (Ok(empty)) from "transient RPC
/// failure" (Err) — the latter must abort the publish so a flaky read
/// cannot silently clear an ops-applied freeze. Uses
/// `get_account_with_commitment` because its return type separates
/// "RPC call succeeded, account absent" from "RPC call failed".
fn read_on_chain_deny_list(
    rpc: &RpcClient,
    pda: &Pubkey,
) -> Result<Vec<[u8; 32]>, Box<dyn std::error::Error + Send + Sync>> {
    let response = rpc.get_account_with_commitment(pda, rpc.commitment())?;
    let acct = match response.value {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    if acct.data.len() < RewardzRoot::LEN {
        return Ok(Vec::new());
    }
    let live = (rroot_read_deny_list_len(&acct.data) as usize).min(REWARDZ_DENY_LIST_MAX);
    let mut out = Vec::with_capacity(live);
    for i in 0..live {
        if let Some(entry) = rroot_read_deny_entry(&acct.data, i as u8) {
            out.push(*entry);
        }
    }
    Ok(out)
}
