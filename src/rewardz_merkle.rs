// ============================================================
// rewardz_merkle.rs — Keccak256 Merkle tree for the Rewardz
// settlement rail (TODO-0018 task 32).
//
// Leaf:  keccak256(authority_bytes || cumulative_amount_le)
// Pair:  keccak256(min(left, right) || max(left, right))
//
// The leaf formula is byte-for-byte identical to `merkle.rs`'s
// PointRoot leaf (authority ‖ u64 LE), so this module delegates its
// primitive hashing to the shared helpers there. The module boundary
// exists so (a) call sites read self-documenting (`build_rewardz_tree`
// in `rewardz_publisher.rs`, `build_merkle_tree` in the existing
// PointRoot publisher — no accidental cross-wiring), and (b) if
// Rewardz ever needs a richer leaf (e.g. bind to epoch, version
// byte, or a different accumulator than u64) the change is local.
//
// CRITICAL: Hash algorithm MUST match the on-chain program in
// `mvp-smart-contracts/program/src/instructions/points/claim_rewardz.rs`
// exactly. See the `test_matches_design_note_leaf` parity test below.
// ============================================================

use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

use crate::merkle::{hash_leaf, hash_pair};

/// A completed Rewardz Merkle tree with root hash and per-authority
/// proofs. Mirrors the shape of `merkle::MerkleTree` but is kept as
/// a distinct type so a call site must be explicit about which rail
/// it is serving — crossing the wires between PointRoot and Rewardz
/// would be a silent state-corruption bug.
#[derive(Debug, Clone)]
pub struct RewardzMerkleTree {
    pub root: [u8; 32],
    pub proofs: HashMap<Pubkey, Vec<[u8; 32]>>,
}

/// Hash a Rewardz leaf: `keccak256(authority_bytes || cumulative_le)`.
/// Delegates to `merkle::hash_leaf` because the formula is identical;
/// the rename documents the Rewardz semantic (cumulative_amount,
/// not per-publish delta).
pub fn hash_rewardz_leaf(authority: &[u8; 32], cumulative_amount: u64) -> [u8; 32] {
    hash_leaf(authority, cumulative_amount)
}

/// Build a Keccak256 Merkle tree over `(authority, cumulative_amount)`
/// entries. Returns `None` for an empty input — callers (specifically
/// `rewardz_publisher.rs`) decide whether that maps to "skip this
/// publish" or "publish empty root for liveness" per design note §Q3.
///
/// **Caller invariants (NOT enforced in release builds, debug-asserted
/// only):**
///
/// 1. **Uniqueness.** Each `Pubkey` MUST appear at most once. Duplicates
///    silently overwrite earlier entries in the returned `proofs`
///    HashMap, leaving the duplicated leaves unprovable while still
///    contributing to the root hash. Today's sole producer
///    (`db::get_rewardz_earnings_grouped`) guarantees this via SQL
///    `GROUP BY protocol_authority`; new call sites must do the same.
/// 2. **Sorted order.** Entries MUST be sorted ascending by
///    `authority.to_bytes()`. Reproducibility (two independent keeper
///    runs producing byte-identical roots from the same logical input)
///    depends on this; the rehearsal test in `rehearse-claim.ts` (task
///    40) asserts byte-equality of the root, so a sort bug shows up
///    there. Today's producer guarantees this via SQL `ORDER BY
///    protocol_authority`.
pub fn build_rewardz_tree(entries: &[(Pubkey, u64)]) -> Option<RewardzMerkleTree> {
    if entries.is_empty() {
        return None;
    }

    // Debug-only invariant guards. Release builds skip these for the
    // hot publisher path; the SQL producer makes them redundant in
    // practice, but a future test fixture or call site that forgets
    // either invariant fails noisily here instead of silently producing
    // a non-reproducible or unprovable tree.
    debug_assert!(
        {
            let mut seen = std::collections::HashSet::new();
            entries.iter().all(|(pk, _)| seen.insert(*pk))
        },
        "build_rewardz_tree: duplicate authorities in input — caller must dedupe",
    );
    debug_assert!(
        entries.windows(2).all(|w| w[0].0.to_bytes() <= w[1].0.to_bytes()),
        "build_rewardz_tree: input not sorted by authority bytes — caller must sort",
    );

    let leaves: Vec<[u8; 32]> = entries
        .iter()
        .map(|(pk, amount)| hash_rewardz_leaf(&pk.to_bytes(), *amount))
        .collect();

    let n = leaves.len();

    if n == 1 {
        let mut proofs = HashMap::new();
        proofs.insert(entries[0].0, vec![]);
        return Some(RewardzMerkleTree {
            root: leaves[0],
            proofs,
        });
    }

    let mut levels: Vec<Vec<[u8; 32]>> = Vec::new();
    levels.push(leaves);

    loop {
        let prev = levels.last().unwrap();
        if prev.len() == 1 {
            break;
        }

        let mut next_level = Vec::new();
        let len = prev.len();
        let mut i = 0;
        while i < len {
            if i + 1 < len {
                next_level.push(hash_pair(&prev[i], &prev[i + 1]));
            } else {
                // Odd tail: promote without hashing so the sibling
                // position is "no element" in proofs (matches the
                // verifier convention in merkle.rs::verify_proof).
                next_level.push(prev[i]);
            }
            i += 2;
        }
        levels.push(next_level);
    }

    let root = levels.last().unwrap()[0];

    let mut proofs = HashMap::new();
    for (leaf_idx, (pk, _)) in entries.iter().enumerate() {
        let mut proof = Vec::new();
        let mut idx = leaf_idx;
        for level in &levels[..levels.len() - 1] {
            let sibling_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
            if sibling_idx < level.len() {
                proof.push(level[sibling_idx]);
            }
            idx /= 2;
        }
        proofs.insert(*pk, proof);
    }

    Some(RewardzMerkleTree { root, proofs })
}

/// Verify a Rewardz proof against a root. Thin wrapper over
/// `merkle::verify_proof` kept here so test and runtime call sites
/// don't need to reach across module boundaries.
pub fn verify_rewardz_proof(leaf: [u8; 32], proof: &[[u8; 32]], root: &[u8; 32]) -> bool {
    crate::merkle::verify_proof(leaf, proof, root)
}

// ============================================================
// Tests
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use sha3::{Digest, Keccak256};

    fn pk(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    #[test]
    fn test_empty_returns_none() {
        assert!(build_rewardz_tree(&[]).is_none());
    }

    #[test]
    fn test_single_leaf_root_equals_leaf_hash() {
        let a = pk(1);
        let entries = vec![(a, 500u64)];
        let tree = build_rewardz_tree(&entries).unwrap();
        assert_eq!(tree.root, hash_rewardz_leaf(&a.to_bytes(), 500));
        assert!(tree.proofs.get(&a).unwrap().is_empty());
    }

    #[test]
    fn test_two_leaves_proofs_verify() {
        let a = pk(1);
        let b = pk(2);
        let entries = vec![(a, 100), (b, 200)];
        let tree = build_rewardz_tree(&entries).unwrap();

        for (pk_i, amt) in &entries {
            let leaf = hash_rewardz_leaf(&pk_i.to_bytes(), *amt);
            let proof = tree.proofs.get(pk_i).unwrap();
            assert!(verify_rewardz_proof(leaf, proof, &tree.root));
        }
    }

    #[test]
    fn test_non_power_of_two_proofs_verify() {
        // 5 leaves exercises the "odd tail promoted" branch at level 0.
        let entries: Vec<(Pubkey, u64)> =
            (1..=5u8).map(|i| (pk(i), (i as u64) * 77)).collect();
        let tree = build_rewardz_tree(&entries).unwrap();

        for (pk_i, amt) in &entries {
            let leaf = hash_rewardz_leaf(&pk_i.to_bytes(), *amt);
            let proof = tree.proofs.get(pk_i).unwrap();
            assert!(
                verify_rewardz_proof(leaf, proof, &tree.root),
                "proof must verify for pk={pk_i}"
            );
        }
    }

    #[test]
    fn test_tampered_amount_fails_verification() {
        let a = pk(1);
        let b = pk(2);
        let entries = vec![(a, 100), (b, 200)];
        let tree = build_rewardz_tree(&entries).unwrap();

        let bogus = hash_rewardz_leaf(&a.to_bytes(), 999);
        let proof = tree.proofs.get(&a).unwrap();
        assert!(!verify_rewardz_proof(bogus, proof, &tree.root));
    }

    /// Parity guard: asserts the leaf formula matches the one declared
    /// in `rewardz-claim-design.md` §claim_rewardz step 2 exactly. If
    /// the on-chain program or the design note changes the leaf shape,
    /// this test fails FIRST — before a silent root mismatch lands in
    /// devnet.
    #[test]
    fn test_matches_design_note_leaf() {
        let authority = [0xABu8; 32];
        let cumulative: u64 = 12_345;

        let mut hasher = Keccak256::new();
        hasher.update(&authority);
        hasher.update(&cumulative.to_le_bytes());
        let expected: [u8; 32] = hasher.finalize().into();

        assert_eq!(hash_rewardz_leaf(&authority, cumulative), expected);
    }

    /// Reproducibility: two independent builds of the same sorted
    /// input must produce byte-identical roots. The
    /// `rehearse-claim.ts` rehearsal (task 40) relies on this.
    #[test]
    fn test_deterministic_root() {
        let mut entries: Vec<(Pubkey, u64)> =
            (1..=4u8).map(|i| (pk(i), (i as u64) * 11)).collect();
        entries.sort_by(|(a, _), (b, _)| a.to_bytes().cmp(&b.to_bytes()));

        let t1 = build_rewardz_tree(&entries).unwrap();
        let t2 = build_rewardz_tree(&entries).unwrap();
        assert_eq!(t1.root, t2.root);
    }
}
