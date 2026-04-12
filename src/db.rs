// ============================================================
// db.rs — PostgreSQL helpers (sqlx)
// ============================================================

use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::Row;

/// Row returned by `get_users_with_points`.
#[derive(Debug, Clone)]
pub struct UserPointsRow {
    pub wallet_address: String,
    pub total_points: i64,
}

/// Row returned by `get_merkle_proof`.
#[derive(Debug, Clone)]
pub struct MerkleProofRow {
    pub root: String,
    pub authority: String,
    pub amount: i64,
    pub proof: Vec<Vec<u8>>,
    pub epoch: i64,
}

/// Create a connection pool.
pub async fn create_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await
}

/// Fetch all users with total_points > 0.
pub async fn get_users_with_points(pool: &PgPool) -> Result<Vec<UserPointsRow>, sqlx::Error> {
    let rows: Vec<PgRow> = sqlx::query(
        "SELECT wallet_address, total_points FROM users WHERE total_points > 0 ORDER BY wallet_address",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| UserPointsRow {
            wallet_address: r.get("wallet_address"),
            total_points: r.get("total_points"),
        })
        .collect())
}

/// Bulk-insert Merkle proofs for an epoch, removing stale proofs first.
pub async fn save_merkle_proofs(
    pool: &PgPool,
    epoch: i64,
    root: &[u8],
    proofs: &[(String, i64, Vec<Vec<u8>>)], // (authority, amount, proof_hashes)
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Encode root as hex string for TEXT column.
    let root_hex = hex_encode(root);

    // Delete proofs from prior epochs (keep only current).
    sqlx::query("DELETE FROM merkle_proofs WHERE epoch < $1")
        .bind(epoch)
        .execute(&mut *tx)
        .await?;

    for (authority, amount, proof_hashes) in proofs {
        // Encode each proof hash as raw bytes for BYTEA[] column.
        let proof_bytea: Vec<Vec<u8>> = proof_hashes.clone();

        sqlx::query(
            r#"
            INSERT INTO merkle_proofs (epoch, root, authority, amount, proof)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (epoch, authority) DO UPDATE
            SET root = EXCLUDED.root, amount = EXCLUDED.amount, proof = EXCLUDED.proof
            "#,
        )
        .bind(epoch)
        .bind(&root_hex)
        .bind(authority)
        .bind(amount)
        .bind(&proof_bytea)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Lookup a single Merkle proof by root hex and authority.
pub async fn get_merkle_proof(
    pool: &PgPool,
    root_hex: &str,
    authority: &str,
) -> Result<Option<MerkleProofRow>, sqlx::Error> {
    let row: Option<PgRow> = sqlx::query(
        "SELECT root, authority, amount, proof, epoch FROM merkle_proofs WHERE root = $1 AND authority = $2",
    )
    .bind(root_hex)
    .bind(authority)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => {
            let root_str: String = r.get("root");
            let authority_str: String = r.get("authority");
            let amount: i64 = r.get("amount");
            let proof_bytea: Vec<Vec<u8>> = r.get("proof");
            let epoch: i64 = r.get("epoch");

            Ok(Some(MerkleProofRow {
                root: root_str,
                authority: authority_str,
                amount,
                proof: proof_bytea,
                epoch,
            }))
        }
        None => Ok(None),
    }
}

/// Get a user's total points.
pub async fn get_user_total_points(
    pool: &PgPool,
    wallet: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<PgRow> =
        sqlx::query("SELECT total_points FROM users WHERE wallet_address = $1")
            .bind(wallet)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.get("total_points")))
}

// ── Hex helpers ─────────────────────────────────────────────

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ()> {
    if s.len() % 2 != 0 {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}
