// ============================================================
// api.rs — HTTP endpoints (Axum)
// ============================================================

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
};
use sqlx::postgres::PgPool;
use std::str::FromStr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

use crate::db;
use crate::rewardz_merkle::build_rewardz_tree;
use crate::rewardz_publisher::read_onchain_rewardz_root;

// ── Shared state ────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub rpc_url: String,
    pub keypair: Arc<Keypair>,
    pub program_id: Pubkey,
    pub points_request_ttl_seconds: i64,
    pub points_receipt_ttl_seconds: i64,
    /// Used by `/rewardz/proof/:authority` to compute the
    /// `valid_until_next_publish_at` hint returned to clients. Mirrors
    /// the cron cadence so the window can never outlast the next
    /// publish — a conservative upper bound, not a guarantee.
    pub rewardz_publish_interval_secs: i64,
}

// ── Request / Response types ────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SignReceiptRequest {
    pub authority: String,
    pub points_claim: u64,
    pub proof: Vec<String>,
    pub root: String,
    pub timestamp: i64,
    pub signature: String,
}

#[derive(Debug, Serialize)]
pub struct SignReceiptResponse {
    pub receipt: Option<Receipt>,
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub prev_total: u64,
    pub new_total: u64,
    pub epoch: u64,
    pub nonce: u64,
    pub expiry: i64,
    pub signature: String,
}

#[derive(Debug, Deserialize)]
pub struct BootstrapAwardRequest {
    pub authority: String,
    pub timestamp: i64,
    pub signature: String,
}

#[derive(Debug, Serialize)]
pub struct BootstrapAwardResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub timestamp: String,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Response body for `GET /rewardz/proof/:authority`. Shape mirrors
/// `rewardz-claim-design.md` §Keeper REST endpoint exactly — any field
/// rename here MUST land in the SDK's `ProtocolAdapter.claimRewardz`
/// consumer in lockstep or the round-trip test breaks.
#[derive(Debug, Serialize)]
pub struct RewardzProofResponse {
    pub cumulative_amount: u64,
    pub epoch: u64,
    /// Hex-encoded (0x-prefixed) sibling hashes, ordered leaf → root.
    pub proof: Vec<String>,
    /// Hex-encoded (0x-prefixed) Keccak256 root — matches the on-chain
    /// `RewardzRoot.root` at the moment this response is produced.
    pub root: String,
    /// Upper bound on when the client should consider the proof stale
    /// and re-fetch. Conservative (= now + publish interval); the
    /// actual staleness boundary is the next successful `set_rewardz_root`
    /// tx, which may fire sooner under operator-initiated rehearsal.
    pub valid_until_next_publish_at: String,
}

// ── Router ──────────────────────────────────────────────────

pub fn create_router(state: AppState, cors_origins: &str) -> Router {
    use tower_http::cors::AllowOrigin;

    let cors = if cors_origins == "*" {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        let origins: Vec<_> = cors_origins
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods(Any)
            .allow_headers(Any)
    };

    Router::new()
        .route("/sign-receipt", post(sign_receipt))
        .route("/bootstrap-award", post(bootstrap_award))
        .route("/rewardz/proof/:authority", get(get_rewardz_proof))
        .route("/healthz", get(healthz))
        .layer(cors)
        .with_state(state)
}

// ── GET /healthz ────────────────────────────────────────────

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    })
}

// ── POST /sign-receipt ──────────────────────────────────────

async fn sign_receipt(
    State(state): State<AppState>,
    Json(req): Json<SignReceiptRequest>,
) -> Result<Json<SignReceiptResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 1. Parse authority pubkey.
    let authority = Pubkey::from_str(&req.authority).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid authority pubkey: {e}"),
            }),
        )
    })?;

    // 2. Verify wallet signature over canonical message.
    let message = format!(
        "rewardz:points-receipt:v1:{}:{}:{}:{}",
        req.authority, req.root, req.points_claim, req.timestamp
    );

    let sig_bytes = bs58::decode(&req.signature).into_vec().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid signature encoding: {e}"),
            }),
        )
    })?;

    let signature = Signature::try_from(sig_bytes.as_slice()).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid signature: {e}"),
            }),
        )
    })?;

    if !signature.verify(authority.as_ref(), message.as_bytes()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid wallet signature".to_string(),
            }),
        ));
    }

    // 3. Check timestamp freshness.
    let now = chrono::Utc::now().timestamp();
    if (now - req.timestamp).abs() > state.points_request_ttl_seconds {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Request timestamp expired".to_string(),
            }),
        ));
    }

    // 4. Fetch the user's current total points from DB.
    let db_total = db::get_user_total_points(&state.pool, &req.authority)
        .await
        .map_err(|e| {
            error!("DB error fetching user points: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Database error".to_string(),
                }),
            )
        })?
        .unwrap_or(0);

    let latest = db_total as u64;

    // 5. Compare latest DB total vs claimed amount.
    if latest == req.points_claim {
        // No new points to sync.
        return Ok(Json(SignReceiptResponse { receipt: None }));
    }

    if latest < req.points_claim {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "Stale DB: claimed points exceed current total".to_string(),
            }),
        ));
    }

    // latest > points_claim: sign a receipt for the new total.
    let expiry = now + state.points_receipt_ttl_seconds;
    let nonce = now as u64; // Simple nonce based on timestamp.

    // Build receipt message for admin signature.
    let receipt_msg = format!(
        "rewardz:receipt:v1:{}:{}:{}:{}:{}",
        req.authority, req.points_claim, latest, nonce, expiry
    );

    let admin_sig = state.keypair.sign_message(receipt_msg.as_bytes());

    // Fetch epoch from on-chain PointRoot if available (best effort, non-blocking).
    let rpc_url_clone = state.rpc_url.clone();
    let pid_clone = state.program_id;
    let epoch = tokio::task::spawn_blocking(move || {
        fetch_current_epoch(&rpc_url_clone, &pid_clone).unwrap_or(0)
    })
    .await
    .unwrap_or(0);

    Ok(Json(SignReceiptResponse {
        receipt: Some(Receipt {
            prev_total: req.points_claim,
            new_total: latest,
            epoch,
            nonce,
            expiry,
            signature: bs58::encode(admin_sig.as_ref()).into_string(),
        }),
    }))
}

// ── POST /bootstrap-award ───────────────────────────────────

async fn bootstrap_award(
    State(state): State<AppState>,
    Json(req): Json<BootstrapAwardRequest>,
) -> Result<Json<BootstrapAwardResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 1. Parse authority.
    let authority = Pubkey::from_str(&req.authority).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid authority pubkey: {e}"),
            }),
        )
    })?;

    // 2. Verify wallet signature.
    let message = format!(
        "rewardz:points-bootstrap-award:v1:{}:{}",
        req.authority, req.timestamp
    );

    let sig_bytes = bs58::decode(&req.signature).into_vec().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid signature encoding: {e}"),
            }),
        )
    })?;

    let signature = Signature::try_from(sig_bytes.as_slice()).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid signature: {e}"),
            }),
        )
    })?;

    if !signature.verify(authority.as_ref(), message.as_bytes()) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid wallet signature".to_string(),
            }),
        ));
    }

    // 3. Check timestamp freshness.
    let now = chrono::Utc::now().timestamp();
    if (now - req.timestamp).abs() > state.points_request_ttl_seconds {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Request timestamp expired".to_string(),
            }),
        ));
    }

    // 4. Check if UserStake already exists on-chain (blocking RPC in spawn_blocking).
    let rpc_url = state.rpc_url.clone();
    let program_id = state.program_id;
    let authority_pk = authority;
    let user_stake_exists = tokio::task::spawn_blocking(move || {
        let rpc = RpcClient::new(&rpc_url);
        let user_stake_seed: &[u8] = b"user_stake";
        let (user_stake_pda, _) =
            Pubkey::find_program_address(&[user_stake_seed, authority_pk.as_ref()], &program_id);
        rpc.get_account(&user_stake_pda).is_ok()
    })
    .await
    .unwrap_or(false);

    if user_stake_exists {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "Already bootstrapped: UserStake account exists".to_string(),
            }),
        ));
    }

    // 5+6. Atomically insert bootstrap points — ON CONFLICT DO NOTHING prevents double-award.
    let bootstrap_amount: i64 = 100;

    let mut tx = state.pool.begin().await.map_err(|e| {
        error!("DB error starting transaction: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Database error".to_string(),
            }),
        )
    })?;

    let result = sqlx::query(
        r#"
        INSERT INTO users (wallet_address, total_points)
        VALUES ($1, $2)
        ON CONFLICT (wallet_address) DO NOTHING
        "#,
    )
    .bind(&req.authority)
    .bind(bootstrap_amount)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        error!("DB error awarding bootstrap points: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Database error".to_string(),
            }),
        )
    })?;

    if result.rows_affected() == 0 {
        // Rollback is implicit on drop, but be explicit.
        let _ = tx.rollback().await;
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "User already exists (bootstrap already applied or has points)".to_string(),
            }),
        ));
    }

    // Also insert into user_balances to keep tables in sync.
    sqlx::query(
        r#"
        INSERT INTO user_balances (wallet_address, total_earned, total_pending, total_spent, total_reserved)
        VALUES ($1, $2, 0, 0, 0)
        ON CONFLICT (wallet_address) DO NOTHING
        "#,
    )
    .bind(&req.authority)
    .bind(bootstrap_amount)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        error!("DB error inserting user_balances: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Database error".to_string(),
            }),
        )
    })?;

    tx.commit().await.map_err(|e| {
        error!("DB error committing bootstrap: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Database error".to_string(),
            }),
        )
    })?;

    info!(
        "Bootstrap award: {} received {} points",
        req.authority, bootstrap_amount
    );

    Ok(Json(BootstrapAwardResponse {
        success: true,
        message: format!("Awarded {bootstrap_amount} bootstrap points"),
    }))
}

// ── GET /rewardz/proof/:authority ───────────────────────────

/// Return the current Merkle proof for a protocol's cumulative Rewardz
/// earnings. The response is consumed by the SDK's
/// `ProtocolAdapter.claimRewardz()` helper — its shape is pinned by
/// `rewardz-claim-design.md` §Keeper REST endpoint.
///
/// Design contract: the returned `root` MUST equal the on-chain
/// `RewardzRoot.root` at the moment of response, because the
/// `claim_rewardz` IX validates the proof against that exact byte
/// string. We therefore:
///   1. Read the on-chain root (single source of truth for what
///      protocols can claim right now).
///   2. Rebuild the tree from current DB state using the same SQL +
///      filter the publisher uses (reproducibility invariant per
///      `rewardz_merkle.rs::test_deterministic_root`).
///   3. Reject with 503 if the rebuilt root doesn't match the on-chain
///      root — that means new `rewardz_earnings` rows have landed
///      since the last publish and the proof we would produce cannot
///      verify on-chain. A subsequent publish closes the gap.
///
/// Persisting proofs in DB at publish time (task 35) will close the
/// 503 window; documented here so the limitation is visible to
/// downstream consumers during the interim.
async fn get_rewardz_proof(
    State(state): State<AppState>,
    Path(authority_str): Path<String>,
) -> Result<Json<RewardzProofResponse>, (StatusCode, Json<ErrorResponse>)> {
    // 1. Parse authority. Keep the parse before any RPC/DB work so
    //    clearly-bad input returns 400 without burning a round trip.
    let target = Pubkey::from_str(&authority_str).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid authority pubkey: {e}"),
            }),
        )
    })?;

    // 2. Read the on-chain RewardzRoot via spawn_blocking — RpcClient
    //    is sync and would otherwise stall the Tokio worker.
    let rpc_url = state.rpc_url.clone();
    let program_id = state.program_id;
    let onchain = tokio::task::spawn_blocking(move || {
        let rpc = RpcClient::new(rpc_url);
        read_onchain_rewardz_root(&rpc, &program_id)
    })
    .await
    .map_err(|e| {
        error!("spawn_blocking failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Internal server error".to_string(),
            }),
        )
    })?
    .map_err(|e| {
        // Transient RPC errors must NOT be collapsed into "proof not
        // available" — clients would retry against a still-unavailable
        // endpoint. 503 signals "try again later".
        warn!("RewardzRoot RPC read failed: {e}");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: format!("Could not read on-chain RewardzRoot: {e}"),
            }),
        )
    })?;

    let (onchain_root, onchain_epoch) = match onchain {
        Some(v) => v,
        None => {
            // Account truly not initialised — publisher hasn't run yet
            // or is still on its first tick. Not a caller error.
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "RewardzRoot not yet initialised on-chain".to_string(),
                }),
            ));
        }
    };

    // 3. Fetch current earnings — same SQL as the publisher so the
    //    rebuilt root can match byte-for-byte.
    let rows = db::get_rewardz_earnings_grouped(&state.pool)
        .await
        .map_err(|e| {
            error!("DB error fetching rewardz earnings: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "Database error".to_string(),
                }),
            )
        })?;

    // Apply the exact same filter as `rewardz_publisher.rs` — a row
    // with a malformed pubkey or non-positive amount cannot be in the
    // tree, and a divergence here would silently produce a root
    // mismatch that looks like "503, try again" forever.
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

    // 4. Build the tree. `None` means zero entries after filter — the
    //    response depends on whether the chain also holds the zero
    //    root (§Q3 liveness) or a real root (state divergence, e.g.
    //    DB wipe after publish). Distinguishing these is the job of
    //    the divergence check below.
    let tree = build_rewardz_tree(&entries);
    let zero_onchain_root = onchain_root == [0u8; 32];

    // 5. State-match check. Covers three cases atomically:
    //    - Zero on-chain root + empty tree → 404 (stable liveness).
    //    - Zero on-chain root + non-empty tree → 503 (publisher behind).
    //    - Non-zero on-chain root + empty tree → 503 (divergence —
    //      DB wipe after publish is the motivating case).
    //    - Non-zero on-chain root + tree.root != onchain → 503
    //      (new earnings since last publish).
    // Task 35 will close the 503 windows by persisting proofs at
    // publish time.
    let tree = match (tree, zero_onchain_root) {
        (None, true) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "No rewardz earnings recorded yet".to_string(),
                }),
            ));
        }
        (None, false) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "Keeper state diverged from chain — DB empty but on-chain root is non-zero".to_string(),
                }),
            ));
        }
        (Some(t), _) => t,
    };

    if tree.root != onchain_root {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Keeper has unpublished earnings — retry after next publish".to_string(),
            }),
        ));
    }

    // 6. Find the target authority's leaf + proof. Build a HashMap of
    //    cumulative amounts from `entries` so lookup is O(1) alongside
    //    the O(1) proof lookup — the tree's entry set can grow with
    //    the protocol cohort over time. Absence means the authority
    //    genuinely has no cumulative earnings.
    let amounts: std::collections::HashMap<Pubkey, u64> = entries.into_iter().collect();
    let cumulative_amount = amounts.get(&target).copied();
    let proof = tree.proofs.get(&target);

    let (cumulative_amount, proof) = match (cumulative_amount, proof) {
        (Some(a), Some(p)) => (a, p),
        _ => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("No rewardz earnings for authority {authority_str}"),
                }),
            ));
        }
    };

    // 7. Assemble response. Hex encode with `0x` prefix to match the
    //    design-note example + existing SDK conventions.
    let proof_hex: Vec<String> = proof
        .iter()
        .map(|h| format!("0x{}", crate::hex::encode(h)))
        .collect();
    let root_hex = format!("0x{}", crate::hex::encode(&onchain_root));

    let valid_until =
        chrono::Utc::now() + chrono::Duration::seconds(state.rewardz_publish_interval_secs);

    Ok(Json(RewardzProofResponse {
        cumulative_amount,
        epoch: onchain_epoch,
        proof: proof_hex,
        root: root_hex,
        valid_until_next_publish_at: valid_until.to_rfc3339(),
    }))
}

// ── Helpers ─────────────────────────────────────────────────

/// Fetch the current epoch from on-chain PointRoot account.
fn fetch_current_epoch(rpc_url: &str, program_id: &Pubkey) -> Option<u64> {
    let rpc = RpcClient::new(rpc_url);
    let point_root_seed: &[u8] = b"point_root";
    let (point_root_pda, _) = Pubkey::find_program_address(&[point_root_seed], program_id);

    match rpc.get_account(&point_root_pda) {
        Ok(account) => {
            let data = &account.data;
            // PointRoot layout: 2-byte prefix + 32 authority + 32 root + 8 epoch
            // epoch offset = 2 + 64 = 66
            if data.len() >= 74 {
                let epoch_bytes: [u8; 8] = data[66..74].try_into().ok()?;
                Some(u64::from_le_bytes(epoch_bytes))
            } else {
                None
            }
        }
        Err(e) => {
            warn!("Could not fetch PointRoot account: {e}");
            None
        }
    }
}
