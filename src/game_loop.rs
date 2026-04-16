// ============================================================
// game_loop.rs — Round-based mining game cranker
// ============================================================
//
// Constants, PDA seeds, discriminators, instruction codes, and
// account layout offsets are imported from the shared
// `rewardz-mvp-api` crate (see mvp-smart-contracts/api). Do NOT
// hardcode them here — they must stay in sync with the on-chain
// program.
//
// IDEMPOTENCY: `start_round` is protected by a preflight guard
// (see `should_skip_start_round` below). setup.sh's bootstrap
// step hands off to the keeper via `cargo run -- full`, and the
// first cron tick submits `start_round` when `current_round_id
// == 0`. If the keeper is restarted or the bootstrap is re-run
// before the first round is settled, repeated invocations must
// be safe no-ops rather than duplicate transactions. TODO-0018
// task 52a owns this contract.

use rewardz_mvp_api::{
    parse_pubkey, parse_u64, validate_account, GAME_CONFIG_SEED, GAME_ROUND_SEED, IX_SETTLE_ROUND,
    IX_START_ROUND, MIN_GAME_PLAYERS, ROUND_VAULT_SEED,
};
use rewardz_mvp_api::state::{GameConfig, GameRound, PlayerDeployment};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::RpcProgramAccountsConfig,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    sysvar,
    transaction::Transaction,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{error, info, warn};

// ── Account layout prefix (version byte + discriminator) ──
// Mirrors `PREFIX_LEN` from the on-chain program: 2 bytes before
// any field offset defined in the api state structs.
const PREFIX_LEN: usize = 2;

fn token_2022_program_id() -> Pubkey {
    Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPwEGpKvp5E8dRmAr91hFq").unwrap()
}

fn system_program_id() -> Pubkey {
    Pubkey::new_from_array([0; 32])
}

#[derive(Debug, Clone)]
struct GameConfigState {
    current_round_id: u64,
    intermission_slots: u64,
    reward_mint: Pubkey,
}

#[derive(Debug, Clone)]
struct GameRoundState {
    round_id: u64,
    end_slot: u64,
    player_count: u32,
    settled: bool,
}

fn game_config_pda(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[GAME_CONFIG_SEED], program_id).0
}

fn game_round_pda(program_id: &Pubkey, round_id: u64) -> Pubkey {
    Pubkey::find_program_address(&[GAME_ROUND_SEED, &round_id.to_le_bytes()], program_id).0
}

fn round_vault_pda(program_id: &Pubkey, round_id: u64) -> Pubkey {
    Pubkey::find_program_address(&[ROUND_VAULT_SEED, &round_id.to_le_bytes()], program_id).0
}

fn read_pubkey(data: &[u8], off: usize) -> Option<Pubkey> {
    parse_pubkey(data, off).map(Pubkey::new_from_array)
}

fn read_u32(data: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?))
}

fn parse_game_config(data: &[u8]) -> Option<GameConfigState> {
    if !validate_account(data, GameConfig::DISCRIMINATOR, GameConfig::LEN) {
        return None;
    }
    Some(GameConfigState {
        current_round_id: parse_u64(data, PREFIX_LEN + GameConfig::OFF_CURRENT_ROUND_ID)?,
        intermission_slots: parse_u64(data, PREFIX_LEN + GameConfig::OFF_INTERMISSION_SLOTS)?,
        reward_mint: read_pubkey(data, PREFIX_LEN + GameConfig::OFF_REWARD_MINT)?,
    })
}

fn parse_game_round(data: &[u8]) -> Option<GameRoundState> {
    if !validate_account(data, GameRound::DISCRIMINATOR, GameRound::LEN) {
        return None;
    }
    Some(GameRoundState {
        round_id: parse_u64(data, PREFIX_LEN + GameRound::OFF_ROUND_ID)?,
        end_slot: parse_u64(data, PREFIX_LEN + GameRound::OFF_END_SLOT)?,
        player_count: read_u32(data, PREFIX_LEN + GameRound::OFF_PLAYER_COUNT)?,
        settled: data.get(PREFIX_LEN + GameRound::OFF_SETTLED).copied()? != 0,
    })
}

fn load_game_config(
    rpc: &RpcClient,
    program_id: &Pubkey,
) -> Result<Option<GameConfigState>, Box<dyn std::error::Error + Send + Sync>> {
    let pda = game_config_pda(program_id);
    match rpc.get_account(&pda) {
        Ok(account) => match parse_game_config(&account.data) {
            Some(config) => Ok(Some(config)),
            None => {
                warn!("GameConfig account {pda} had unexpected layout");
                Ok(None)
            }
        },
        Err(e) => {
            warn!("GameConfig account {pda} unavailable; initialize game first: {e}");
            Ok(None)
        }
    }
}

fn load_game_round(
    rpc: &RpcClient,
    program_id: &Pubkey,
    round_id: u64,
) -> Result<Option<GameRoundState>, Box<dyn std::error::Error + Send + Sync>> {
    let pda = game_round_pda(program_id, round_id);
    match rpc.get_account(&pda) {
        Ok(account) => match parse_game_round(&account.data) {
            Some(round) => Ok(Some(round)),
            None => {
                warn!("GameRound account {pda} had unexpected layout");
                Ok(None)
            }
        },
        Err(e) => {
            warn!("GameRound account {pda} unavailable for round {round_id}: {e}");
            Ok(None)
        }
    }
}

fn load_deployments(
    rpc: &RpcClient,
    program_id: &Pubkey,
    round_id: u64,
) -> Result<Vec<Pubkey>, Box<dyn std::error::Error + Send + Sync>> {
    let mut accounts = rpc
        .get_program_accounts_with_config(
            program_id,
            RpcProgramAccountsConfig {
                filters: Some(vec![
                    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                        0,
                        vec![PlayerDeployment::DISCRIMINATOR],
                    )),
                    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                        PREFIX_LEN + PlayerDeployment::OFF_ROUND_ID,
                        round_id.to_le_bytes().to_vec(),
                    )),
                ]),
                account_config: Default::default(),
                with_context: None,
                sort_results: None,
            },
        )?
        .into_iter()
        .map(|(pubkey, _account)| pubkey)
        .collect::<Vec<_>>();
    accounts.sort_by_key(|p| p.to_string());
    Ok(accounts)
}

fn send_instruction(
    rpc: &RpcClient,
    keypair: &Keypair,
    instruction: Instruction,
) -> Result<Signature, Box<dyn std::error::Error + Send + Sync>> {
    let recent_blockhash = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&keypair.pubkey()),
        &[keypair],
        recent_blockhash,
    );
    Ok(rpc.send_and_confirm_transaction(&tx)?)
}

// ── Idempotency guard for start_round ─────────────────────────
//
// `setup.sh` hands off to the keeper via `cargo run -- full`, and the
// very first `tick_game_loop` invocation detects `current_round_id == 0`
// and submits a `start_round` transaction. Any repeated bootstrap
// (re-running setup, restarting the keeper mid-round, a cron tick that
// races a prior tick whose tx has not yet landed in the caller's RPC
// view) must be a safe no-op instead of producing a duplicate
// transaction that the on-chain program would reject.
//
// The pure predicate below is kept separate from the RPC-touching
// wrapper so it can be unit-tested without a live validator.
//
// Contract: if the *target* round PDA (i.e. the one `start_round` is
// about to create, at `current_round_id + 1`) already exists and is
// not yet `settled`, the round is considered active and we skip.
// Any other state (PDA missing, or present but settled) falls through
// to normal tx submission.
fn should_skip_start_round(round_state: Option<&GameRoundState>) -> bool {
    match round_state {
        Some(round) => !round.settled,
        None => false,
    }
}

/// Outcome of a `start_round` attempt.
///
/// `Submitted` carries the on-chain signature for the freshly opened
/// round. `Skipped` means the guard fired — the target round PDA was
/// already active, so no transaction was sent. Callers log differently
/// in each case so operators can distinguish bootstrap races from
/// genuine round openings.
#[derive(Debug)]
enum StartRoundOutcome {
    Submitted(Signature),
    Skipped { round_id: u64 },
}

fn start_round(
    rpc: &RpcClient,
    keypair: &Keypair,
    program_id: &Pubkey,
    config: &GameConfigState,
) -> Result<StartRoundOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let next_round_id = config
        .current_round_id
        .checked_add(1)
        .ok_or("round id overflow")?;

    // Preflight: one free account read to detect "round already active".
    // If the target PDA exists and is not settled, another tick (or an
    // earlier bootstrap attempt) beat us to it. Silently no-op.
    let existing = load_game_round(rpc, program_id, next_round_id)?;
    if should_skip_start_round(existing.as_ref()) {
        info!(
            "start_round skipped: round {next_round_id} already active (idempotency guard)"
        );
        return Ok(StartRoundOutcome::Skipped {
            round_id: next_round_id,
        });
    }

    let sig = submit_start_round(rpc, keypair, program_id, config, next_round_id)?;
    Ok(StartRoundOutcome::Submitted(sig))
}

fn submit_start_round(
    rpc: &RpcClient,
    keypair: &Keypair,
    program_id: &Pubkey,
    config: &GameConfigState,
    next_round_id: u64,
) -> Result<Signature, Box<dyn std::error::Error + Send + Sync>> {
    let game_config = game_config_pda(program_id);
    let game_round = game_round_pda(program_id, next_round_id);
    let previous_round = if config.current_round_id > 0 {
        game_round_pda(program_id, config.current_round_id)
    } else {
        system_program_id()
    };
    let round_vault = round_vault_pda(program_id, next_round_id);
    let ix = Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(keypair.pubkey(), true),
            AccountMeta::new(game_config, false),
            AccountMeta::new(game_round, false),
            AccountMeta::new_readonly(previous_round, false),
            AccountMeta::new_readonly(config.reward_mint, false),
            AccountMeta::new(round_vault, false),
            AccountMeta::new_readonly(system_program_id(), false),
            AccountMeta::new_readonly(token_2022_program_id(), false),
        ],
        data: vec![IX_START_ROUND],
    };
    send_instruction(rpc, keypair, ix)
}

fn settle_round(
    rpc: &RpcClient,
    keypair: &Keypair,
    program_id: &Pubkey,
    config: &GameConfigState,
    round: &GameRoundState,
) -> Result<Signature, Box<dyn std::error::Error + Send + Sync>> {
    let deployments = load_deployments(rpc, program_id, round.round_id)?;
    if deployments.len() != round.player_count as usize {
        warn!(
            "Round {} player count is {}, but {} deployment PDA(s) were found",
            round.round_id,
            round.player_count,
            deployments.len()
        );
    }
    if round.player_count < MIN_GAME_PLAYERS {
        info!(
            "Settling round {} as skipped; only {} player(s)",
            round.round_id, round.player_count
        );
    } else {
        info!(
            "Settling round {} with {} player deployment(s)",
            round.round_id,
            deployments.len()
        );
    }

    // SettleRound is an O(1) snapshot — no PlayerDeployment accounts required.
    // Per-player hit computation happens in CheckpointRound (see cranker loop).
    let accounts = vec![
        AccountMeta::new_readonly(keypair.pubkey(), true),
        AccountMeta::new(game_config_pda(program_id), false),
        AccountMeta::new(game_round_pda(program_id, round.round_id), false),
        AccountMeta::new(config.reward_mint, false),
        AccountMeta::new(round_vault_pda(program_id, round.round_id), false),
        AccountMeta::new_readonly(sysvar::slot_hashes::id(), false),
        AccountMeta::new_readonly(token_2022_program_id(), false),
    ];

    let ix = Instruction {
        program_id: *program_id,
        accounts,
        data: vec![IX_SETTLE_ROUND],
    };
    send_instruction(rpc, keypair, ix)
}

/// One tick of the game loop. Performs AT MOST one on-chain state
/// transition per invocation (start, or settle). A settled round
/// waiting for intermission will cause the next tick to start a new
/// round. Splitting settle and start into separate ticks keeps each
/// transaction isolated and idempotent.
pub async fn tick_game_loop(
    rpc: &RpcClient,
    keypair: &Keypair,
    program_id: &Pubkey,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(config) = load_game_config(rpc, program_id)? else {
        return Ok(());
    };
    let current_slot = rpc.get_slot()?;

    if config.current_round_id == 0 {
        match start_round(rpc, keypair, program_id, &config)? {
            StartRoundOutcome::Submitted(sig) => {
                info!("Started first mining round, tx={sig}");
            }
            StartRoundOutcome::Skipped { round_id } => {
                info!(
                    "First mining round {round_id} already active; bootstrap no-op"
                );
            }
        }
        return Ok(());
    }

    let Some(round) = load_game_round(rpc, program_id, config.current_round_id)? else {
        return Ok(());
    };
    let crank_slot = round
        .end_slot
        .checked_add(config.intermission_slots)
        .ok_or("round crank slot overflow")?;

    if round.settled {
        if current_slot >= crank_slot {
            match start_round(rpc, keypair, program_id, &config)? {
                StartRoundOutcome::Submitted(sig) => {
                    info!(
                        "Started mining round {} after settled round {}, tx={sig}",
                        config.current_round_id + 1,
                        round.round_id
                    );
                }
                StartRoundOutcome::Skipped { round_id } => {
                    info!(
                        "Round {round_id} already active; skipping redundant start_round after settled round {}",
                        round.round_id
                    );
                }
            }
        } else {
            info!(
                "Round {} settled; next round opens at slot {} (current {})",
                round.round_id, crank_slot, current_slot
            );
        }
        return Ok(());
    }

    if current_slot < crank_slot {
        info!(
            "Round {} active; end_slot={}, crank_slot={}, current_slot={}, players={}",
            round.round_id, round.end_slot, crank_slot, current_slot, round.player_count
        );
        return Ok(());
    }

    // Single-round tick: only settle the active round. The next tick
    // will observe `settled == true` and start the next round. This
    // bounds each tick to a single transaction.
    let settle_sig = settle_round(rpc, keypair, program_id, &config, &round)?;
    info!(
        "Settled mining round {}, tx={settle_sig}. Next round will start on the following tick.",
        round.round_id
    );
    Ok(())
}

pub fn start_game_loop_cron(
    interval_secs: u64,
    rpc_url: String,
    keypair: Arc<Keypair>,
    program_id: Pubkey,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        let rpc = RpcClient::new(rpc_url);

        interval.tick().await;

        loop {
            interval.tick().await;
            if let Err(e) = tick_game_loop(&rpc, &keypair, &program_id).await {
                error!("Game loop cron error: {e}");
            }
        }
    });
}

/// Opt-in per-player checkpoint cranker. Gated on env var
/// `KEEPER_RUN_CRANKER=true`; default is off so existing deployments
/// see no behavioural change. When enabled, this loop will (in future
/// revisions) scan settled rounds for outstanding PlayerDeployment
/// accounts and submit `CheckpointRound` transactions, collecting the
/// checkpoint fee per on-chain program rules. Today it simply logs
/// its heartbeat — scope is limited to wiring the env-var gate so
/// downstream tasks can light it up without touching the keeper main.
pub fn start_cranker_loop(
    interval_secs: u64,
    _rpc_url: String,
    _keypair: Arc<Keypair>,
    _program_id: Pubkey,
) {
    let enabled = std::env::var("KEEPER_RUN_CRANKER")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false);

    if !enabled {
        info!("Cranker loop disabled (set KEEPER_RUN_CRANKER=true to enable)");
        return;
    }

    info!("Cranker loop enabled; scanning settled rounds every {interval_secs}s");
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        interval.tick().await;
        loop {
            interval.tick().await;
            // Placeholder: per-player CheckpointRound cranking lands in a
            // follow-up task. Keeping this as a no-op heartbeat preserves
            // the env-var contract without altering on-chain behaviour.
            info!("cranker tick (noop; checkpoint cranking not yet implemented)");
        }
    });
}

// ============================================================
// Tests
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn make_round(round_id: u64, settled: bool) -> GameRoundState {
        GameRoundState {
            round_id,
            end_slot: 100,
            player_count: 0,
            settled,
        }
    }

    #[test]
    fn should_skip_when_target_round_active() {
        // Round PDA exists and is NOT settled → start_round must be a no-op.
        // This is the bootstrap-race case: setup.sh handoff → keeper cron
        // observes current_round_id==0 → submits start_round → before the
        // tx is visible, a second tick (or restart) tries again.
        let active = make_round(1, false);
        assert!(
            should_skip_start_round(Some(&active)),
            "active round must trigger the idempotency guard"
        );
    }

    #[test]
    fn should_not_skip_when_target_round_absent() {
        // Round PDA does not exist yet (typical first-ever bootstrap).
        // Must fall through to tx submission.
        assert!(
            !should_skip_start_round(None),
            "missing round PDA must allow start_round to submit"
        );
    }

    #[test]
    fn should_not_skip_when_target_round_settled() {
        // Round PDA exists AND is settled (i.e. the round that just
        // finished). `start_round` with next_round_id pointing at a
        // settled PDA would be a pathological reuse — in practice the
        // caller passes next_round_id = current_round_id + 1, so the
        // PDA shouldn't exist at all. Defensive: if it does exist and
        // is settled, we still allow submission to surface the on-chain
        // error rather than silently swallowing it.
        let done = make_round(1, true);
        assert!(
            !should_skip_start_round(Some(&done)),
            "settled round must not trigger the guard"
        );
    }

    #[test]
    fn start_round_outcome_reports_skip_round_id() {
        // Smoke test for the outcome enum — ensures `Skipped` carries
        // the round id so log lines can identify which round was
        // already active.
        let outcome = StartRoundOutcome::Skipped { round_id: 42 };
        match outcome {
            StartRoundOutcome::Skipped { round_id } => assert_eq!(round_id, 42),
            StartRoundOutcome::Submitted(_) => panic!("expected Skipped variant"),
        }
    }
}
