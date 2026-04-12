// ============================================================
// game_loop.rs — Round-based mining game cranker
// ============================================================

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

const DISC_GAME_CONFIG: u8 = 7;
const DISC_GAME_ROUND: u8 = 8;
const DISC_PLAYER_DEPLOYMENT: u8 = 9;
const IX_START_ROUND: u8 = 19;
const IX_SETTLE_ROUND: u8 = 21;
const MIN_GAME_PLAYERS: u32 = 2;

const GAME_CONFIG_SEED: &[u8] = b"game_config";
const GAME_ROUND_SEED: &[u8] = b"game_round";
const ROUND_VAULT_SEED: &[u8] = b"round_vault";

const PREFIX_LEN: usize = 2;
const GAME_CONFIG_LEN: usize = 193;
const GAME_CONFIG_OFF_REWARD_MINT: usize = PREFIX_LEN + 32;
const GAME_CONFIG_OFF_CURRENT_ROUND_ID: usize = PREFIX_LEN + 128;
const GAME_CONFIG_OFF_INTERMISSION_SLOTS: usize = PREFIX_LEN + 144;
const GAME_ROUND_LEN: usize = 168;
const GAME_ROUND_OFF_ROUND_ID: usize = PREFIX_LEN;
const GAME_ROUND_OFF_END_SLOT: usize = PREFIX_LEN + 16;
const GAME_ROUND_OFF_PLAYER_COUNT: usize = PREFIX_LEN + 24;
const GAME_ROUND_OFF_SETTLED: usize = PREFIX_LEN + 44;
const PLAYER_DEPLOYMENT_OFF_ROUND_ID: usize = PREFIX_LEN + 32;

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

fn read_u64(data: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(off..off + 8)?.try_into().ok()?))
}

fn read_u32(data: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?))
}

fn read_pubkey(data: &[u8], off: usize) -> Option<Pubkey> {
    let bytes: [u8; 32] = data.get(off..off + 32)?.try_into().ok()?;
    Some(Pubkey::new_from_array(bytes))
}

fn parse_game_config(data: &[u8]) -> Option<GameConfigState> {
    if data.len() < GAME_CONFIG_LEN || data[0] != DISC_GAME_CONFIG {
        return None;
    }
    Some(GameConfigState {
        current_round_id: read_u64(data, GAME_CONFIG_OFF_CURRENT_ROUND_ID)?,
        intermission_slots: read_u64(data, GAME_CONFIG_OFF_INTERMISSION_SLOTS)?,
        reward_mint: read_pubkey(data, GAME_CONFIG_OFF_REWARD_MINT)?,
    })
}

fn parse_game_round(data: &[u8]) -> Option<GameRoundState> {
    if data.len() < GAME_ROUND_LEN || data[0] != DISC_GAME_ROUND {
        return None;
    }
    Some(GameRoundState {
        round_id: read_u64(data, GAME_ROUND_OFF_ROUND_ID)?,
        end_slot: read_u64(data, GAME_ROUND_OFF_END_SLOT)?,
        player_count: read_u32(data, GAME_ROUND_OFF_PLAYER_COUNT)?,
        settled: data.get(GAME_ROUND_OFF_SETTLED).copied()? != 0,
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
                    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, vec![DISC_PLAYER_DEPLOYMENT])),
                    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                        PLAYER_DEPLOYMENT_OFF_ROUND_ID,
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

fn start_round(
    rpc: &RpcClient,
    keypair: &Keypair,
    program_id: &Pubkey,
    config: &GameConfigState,
) -> Result<Signature, Box<dyn std::error::Error + Send + Sync>> {
    let next_round_id = config
        .current_round_id
        .checked_add(1)
        .ok_or("round id overflow")?;
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

    let mut accounts = vec![
        AccountMeta::new_readonly(keypair.pubkey(), true),
        AccountMeta::new(game_config_pda(program_id), false),
        AccountMeta::new(game_round_pda(program_id, round.round_id), false),
        AccountMeta::new(config.reward_mint, false),
        AccountMeta::new(round_vault_pda(program_id, round.round_id), false),
        AccountMeta::new_readonly(sysvar::slot_hashes::id(), false),
        AccountMeta::new_readonly(token_2022_program_id(), false),
    ];
    accounts.extend(
        deployments
            .into_iter()
            .map(|pda| AccountMeta::new(pda, false)),
    );

    let ix = Instruction {
        program_id: *program_id,
        accounts,
        data: vec![IX_SETTLE_ROUND],
    };
    send_instruction(rpc, keypair, ix)
}

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
        let sig = start_round(rpc, keypair, program_id, &config)?;
        info!("Started first mining round, tx={sig}");
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
            let sig = start_round(rpc, keypair, program_id, &config)?;
            info!(
                "Started mining round {} after settled round {}, tx={sig}",
                config.current_round_id + 1,
                round.round_id
            );
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

    let settle_sig = settle_round(rpc, keypair, program_id, &config, &round)?;
    info!("Settled mining round {}, tx={settle_sig}", round.round_id);

    let start_sig = start_round(rpc, keypair, program_id, &config)?;
    info!(
        "Started mining round {} after settlement, tx={start_sig}",
        config.current_round_id + 1
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
