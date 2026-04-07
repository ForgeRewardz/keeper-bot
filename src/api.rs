// ============================================================
// api.rs — HTTP endpoints (Axum)
// ============================================================

use axum::{
    extract::State,
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

// ── Shared state ────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub rpc_url: String,
    pub keypair: Arc<Keypair>,
    pub program_id: Pubkey,
    pub points_request_ttl_seconds: i64,
    pub points_receipt_ttl_seconds: i64,
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
