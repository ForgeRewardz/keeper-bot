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
    let root_hex = crate::hex::encode(root);

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

/// Row returned by `get_rewardz_earnings_grouped`.
#[derive(Debug, Clone)]
pub struct RewardzEarningsRow {
    pub protocol_authority: String,
    pub cumulative_amount: i64,
}

/// Return one row per protocol_authority with the cumulative sum of all
/// Rewardz earnings ever recorded for that authority. Mirrors design note
/// `rewardz-claim-design.md` §Keeper cron step 1 exactly — no
/// `included_in_root_epoch` filter, because the leaf formula is the
/// cumulative (not per-epoch delta) amount. Ordered by authority so two
/// independent keeper runs against the same data build the same Merkle
/// tree byte-for-byte.
pub async fn get_rewardz_earnings_grouped(
    pool: &PgPool,
) -> Result<Vec<RewardzEarningsRow>, sqlx::Error> {
    let rows: Vec<PgRow> = sqlx::query(
        r#"
        SELECT protocol_authority, SUM(amount)::BIGINT AS total
        FROM rewardz_earnings
        GROUP BY protocol_authority
        ORDER BY protocol_authority
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| RewardzEarningsRow {
            protocol_authority: r.get("protocol_authority"),
            cumulative_amount: r.get("total"),
        })
        .collect())
}

/// Record a successful Rewardz root publish: insert into
/// `rewardz_root_epochs` and backfill `included_in_root_epoch` on the
/// earnings rows that were folded into the tree. Runs in a single
/// transaction so the two writes either both land or both rollback —
/// the pair is what makes `rewardz_root_epochs` a faithful record of
/// what's on-chain.
///
/// # Idempotency
///
/// - `ON CONFLICT (epoch) DO NOTHING` on the root-epoch insert is
///   defence-in-depth against a future same-epoch retry path (e.g. a
///   boot-time reconciliation that compares `rewardz_root_epochs.max(epoch)`
///   against on-chain epoch and replays missing backfills). No such
///   reconciliation exists today — the cron computes `next_epoch =
///   on-chain + 1`, so a crash between `send_and_confirm` and this
///   write leaves the confirmed epoch permanently unbackfilled and
///   its rows roll into a future epoch. The ON CONFLICT guard simply
///   ensures that if reconciliation is added later, it cannot break.
/// - `WHERE included_in_root_epoch IS NULL` on the backfill prevents
///   re-stamping rows already marked for an earlier epoch — a row's
///   `included_in_root_epoch` records the FIRST epoch that included
///   it, which is what downstream observability wants.
///
/// # `NUMERIC` binding
///
/// `total_included` is u128 on the caller side; sqlx has no native
/// u128 encoder. We bind as TEXT + cast via `$4::NUMERIC` so values
/// above i64::MAX round-trip losslessly if the per-epoch cap is ever
/// raised beyond 9.2 × 10^18.
///
/// # `authorities` filter
///
/// Caller passes the exact set of protocol_authority values that went
/// into the tree (matches the publisher's filter for malformed pubkey
/// and non-positive cumulative). Rows belonging to filtered-out
/// authorities stay unmarked — they can be swept into a later epoch
/// once their cumulative crosses zero.
///
/// # `cutoff` race-window guard
///
/// `cutoff` is captured from the DB clock BEFORE the publisher reads
/// earnings, and the backfill UPDATE constrains `created_at <= cutoff`
/// so rows inserted AFTER the tree was built cannot be stamped with
/// this epoch's number. Without this guard a concurrent INSERT sharing
/// a tree authority would inherit this epoch despite not contributing
/// to its on-chain cumulative — the claim rail is unaffected (claims
/// are cumulative) but the marker's "FIRST epoch that included it"
/// contract would be imprecise.
pub async fn record_rewardz_root_publish(
    pool: &PgPool,
    epoch: i64,
    merkle_root: &[u8; 32],
    tx_sig: &str,
    total_included: u128,
    authorities: &[String],
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        INSERT INTO rewardz_root_epochs (epoch, merkle_root, tx_sig, total_included)
        VALUES ($1, $2, $3, $4::NUMERIC)
        ON CONFLICT (epoch) DO NOTHING
        "#,
    )
    .bind(epoch)
    .bind(&merkle_root[..])
    .bind(tx_sig)
    .bind(total_included.to_string())
    .execute(&mut *tx)
    .await?;

    let result = sqlx::query(
        r#"
        UPDATE rewardz_earnings
        SET included_in_root_epoch = $1
        WHERE included_in_root_epoch IS NULL
          AND protocol_authority = ANY($2)
          AND created_at <= $3
        "#,
    )
    .bind(epoch)
    .bind(authorities)
    .bind(cutoff)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(result.rows_affected())
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

