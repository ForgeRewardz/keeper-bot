// ============================================================
// merkle.rs — Merkle tree library
//
// CRITICAL: Hash algorithm MUST match the on-chain program in
// mvp-smart-contracts/program/src/points.rs exactly.
//
// Leaf:  keccak256(authority_bytes || points_le_bytes)
// Pair:  keccak256(min(left, right) || max(left, right))
// ============================================================

use sha3::{Digest, Keccak256};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

/// A completed Merkle tree with root hash and per-leaf proofs.
#[derive(Debug, Clone)]
pub struct MerkleTree {
    pub root: [u8; 32],
    pub proofs: HashMap<Pubkey, Vec<[u8; 32]>>,
}

/// Hash a leaf node: keccak256(authority_bytes || points_le_bytes).
pub fn hash_leaf(authority: &[u8; 32], points: u64) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(authority);
    hasher.update(&points.to_le_bytes());
    hasher.finalize().into()
}

/// Hash an internal pair: keccak256(min(a, b) || max(a, b)).
/// Sorting prevents tree malleability.
pub fn hash_pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    if a <= b {
        hasher.update(a);
        hasher.update(b);
    } else {
        hasher.update(b);
        hasher.update(a);
    }
    hasher.finalize().into()
}

/// Build a Merkle tree from a list of (pubkey, points) entries.
///
/// Returns `None` if the input is empty.
pub fn build_merkle_tree(entries: &[(Pubkey, u64)]) -> Option<MerkleTree> {
    if entries.is_empty() {
        return None;
    }

    // Compute leaf hashes.
    let leaves: Vec<[u8; 32]> = entries
        .iter()
        .map(|(pk, pts)| hash_leaf(&pk.to_bytes(), *pts))
        .collect();

    let n = leaves.len();

    if n == 1 {
        let mut proofs = HashMap::new();
        proofs.insert(entries[0].0, vec![]);
        return Some(MerkleTree {
            root: leaves[0],
            proofs,
        });
    }

    // Build tree level by level, tracking indices for proof extraction.
    // We store each level of the tree from leaves up to root.
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
                // Odd node: promote it (hash with itself).
                next_level.push(prev[i]);
            }
            i += 2;
        }
        levels.push(next_level);
    }

    let root = levels.last().unwrap()[0];

    // Build proofs for each leaf by walking back up the tree.
    let mut proofs = HashMap::new();
    for (leaf_idx, (pk, _)) in entries.iter().enumerate() {
        let mut proof = Vec::new();
        let mut idx = leaf_idx;

        for level in &levels[..levels.len() - 1] {
            let sibling_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
            if sibling_idx < level.len() {
                proof.push(level[sibling_idx]);
            }
            // No sibling means odd node was promoted — no proof element needed.
            idx /= 2;
        }

        proofs.insert(*pk, proof);
    }

    Some(MerkleTree { root, proofs })
}

/// Verify a Merkle proof against a root.
pub fn verify_proof(leaf: [u8; 32], proof: &[[u8; 32]], root: &[u8; 32]) -> bool {
    let mut current = leaf;
    for element in proof {
        current = hash_pair(&current, element);
    }
    &current == root
}

// ============================================================
// Tests
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn test_pubkey(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    #[test]
    fn test_hash_leaf_deterministic() {
        let authority = [1u8; 32];
        let h1 = hash_leaf(&authority, 100);
        let h2 = hash_leaf(&authority, 100);
        assert_eq!(h1, h2, "Same inputs must produce same hash");
    }

    #[test]
    fn test_hash_leaf_different_points() {
        let authority = [1u8; 32];
        let h1 = hash_leaf(&authority, 100);
        let h2 = hash_leaf(&authority, 200);
        assert_ne!(h1, h2, "Different points must produce different hashes");
    }

    #[test]
    fn test_hash_leaf_different_authority() {
        let h1 = hash_leaf(&[1u8; 32], 100);
        let h2 = hash_leaf(&[2u8; 32], 100);
        assert_ne!(h1, h2, "Different authorities must produce different hashes");
    }

    #[test]
    fn test_hash_pair_sorted() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let h1 = hash_pair(&a, &b);
        let h2 = hash_pair(&b, &a);
        assert_eq!(h1, h2, "Sorted pair hashing must be commutative");
    }

    #[test]
    fn test_hash_pair_same_inputs() {
        let a = [5u8; 32];
        let h = hash_pair(&a, &a);
        // hash_pair with identical inputs: keccak256(a || a) since a <= a
        let mut hasher = Keccak256::new();
        hasher.update(&a);
        hasher.update(&a);
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(h, expected);
    }

    #[test]
    fn test_empty_entries() {
        let result = build_merkle_tree(&[]);
        assert!(result.is_none(), "Empty entries must return None");
    }

    #[test]
    fn test_single_leaf() {
        let pk = test_pubkey(1);
        let entries = vec![(pk, 500u64)];
        let tree = build_merkle_tree(&entries).expect("single leaf tree");

        // Root should equal the leaf hash.
        let expected_root = hash_leaf(&pk.to_bytes(), 500);
        assert_eq!(tree.root, expected_root, "Single leaf root = leaf hash");

        // Proof should be empty.
        let proof = tree.proofs.get(&pk).expect("proof exists");
        assert!(proof.is_empty(), "Single leaf proof is empty");

        // Verify proof.
        assert!(verify_proof(expected_root, proof, &tree.root));
    }

    #[test]
    fn test_two_leaves() {
        let pk1 = test_pubkey(1);
        let pk2 = test_pubkey(2);
        let entries = vec![(pk1, 100), (pk2, 200)];
        let tree = build_merkle_tree(&entries).expect("two leaf tree");

        let leaf1 = hash_leaf(&pk1.to_bytes(), 100);
        let leaf2 = hash_leaf(&pk2.to_bytes(), 200);
        let expected_root = hash_pair(&leaf1, &leaf2);
        assert_eq!(tree.root, expected_root, "Two leaf root = hash_pair(l1, l2)");

        // Proof for pk1 should be [leaf2].
        let proof1 = tree.proofs.get(&pk1).expect("proof1 exists");
        assert_eq!(proof1.len(), 1);
        assert_eq!(proof1[0], leaf2);
        assert!(verify_proof(leaf1, proof1, &tree.root));

        // Proof for pk2 should be [leaf1].
        let proof2 = tree.proofs.get(&pk2).expect("proof2 exists");
        assert_eq!(proof2.len(), 1);
        assert_eq!(proof2[0], leaf1);
        assert!(verify_proof(leaf2, proof2, &tree.root));
    }

    #[test]
    fn test_four_leaves_power_of_2() {
        let entries: Vec<(Pubkey, u64)> = (1..=4u8)
            .map(|i| (test_pubkey(i), i as u64 * 100))
            .collect();
        let tree = build_merkle_tree(&entries).expect("four leaf tree");

        // Every leaf proof should verify.
        for (pk, pts) in &entries {
            let leaf = hash_leaf(&pk.to_bytes(), *pts);
            let proof = tree.proofs.get(pk).expect("proof exists");
            assert!(
                verify_proof(leaf, proof, &tree.root),
                "Proof must verify for pk={pk}"
            );
        }
    }

    #[test]
    fn test_eight_leaves_power_of_2() {
        let entries: Vec<(Pubkey, u64)> = (1..=8u8)
            .map(|i| (test_pubkey(i), i as u64 * 50))
            .collect();
        let tree = build_merkle_tree(&entries).expect("eight leaf tree");

        for (pk, pts) in &entries {
            let leaf = hash_leaf(&pk.to_bytes(), *pts);
            let proof = tree.proofs.get(pk).expect("proof exists");
            assert!(
                verify_proof(leaf, proof, &tree.root),
                "Proof must verify for pk={pk}"
            );
        }
    }

    #[test]
    fn test_three_leaves_non_power_of_2() {
        let entries: Vec<(Pubkey, u64)> = (1..=3u8)
            .map(|i| (test_pubkey(i), i as u64 * 100))
            .collect();
        let tree = build_merkle_tree(&entries).expect("three leaf tree");

        for (pk, pts) in &entries {
            let leaf = hash_leaf(&pk.to_bytes(), *pts);
            let proof = tree.proofs.get(pk).expect("proof exists");
            assert!(
                verify_proof(leaf, proof, &tree.root),
                "Proof must verify for pk={pk}"
            );
        }
    }

    #[test]
    fn test_five_leaves_non_power_of_2() {
        let entries: Vec<(Pubkey, u64)> = (1..=5u8)
            .map(|i| (test_pubkey(i), i as u64 * 100))
            .collect();
        let tree = build_merkle_tree(&entries).expect("five leaf tree");

        for (pk, pts) in &entries {
            let leaf = hash_leaf(&pk.to_bytes(), *pts);
            let proof = tree.proofs.get(pk).expect("proof exists");
            assert!(
                verify_proof(leaf, proof, &tree.root),
                "Proof must verify for pk={pk}"
            );
        }
    }

    #[test]
    fn test_seven_leaves_non_power_of_2() {
        let entries: Vec<(Pubkey, u64)> = (1..=7u8)
            .map(|i| (test_pubkey(i), i as u64 * 100))
            .collect();
        let tree = build_merkle_tree(&entries).expect("seven leaf tree");

        for (pk, pts) in &entries {
            let leaf = hash_leaf(&pk.to_bytes(), *pts);
            let proof = tree.proofs.get(pk).expect("proof exists");
            assert!(
                verify_proof(leaf, proof, &tree.root),
                "Proof must verify for pk={pk}"
            );
        }
    }

    #[test]
    fn test_verify_proof_with_wrong_root_fails() {
        let pk1 = test_pubkey(1);
        let pk2 = test_pubkey(2);
        let entries = vec![(pk1, 100), (pk2, 200)];
        let tree = build_merkle_tree(&entries).expect("tree");

        let leaf1 = hash_leaf(&pk1.to_bytes(), 100);
        let proof1 = tree.proofs.get(&pk1).expect("proof");
        let bad_root = [0xFFu8; 32];
        assert!(
            !verify_proof(leaf1, proof1, &bad_root),
            "Proof against wrong root must fail"
        );
    }

    #[test]
    fn test_verify_proof_with_wrong_leaf_fails() {
        let pk1 = test_pubkey(1);
        let pk2 = test_pubkey(2);
        let entries = vec![(pk1, 100), (pk2, 200)];
        let tree = build_merkle_tree(&entries).expect("tree");

        let wrong_leaf = hash_leaf(&pk1.to_bytes(), 999); // wrong points
        let proof1 = tree.proofs.get(&pk1).expect("proof");
        assert!(
            !verify_proof(wrong_leaf, proof1, &tree.root),
            "Proof with wrong leaf must fail"
        );
    }

    /// Verify the on-chain hash algorithm is matched exactly.
    /// This test manually computes what the on-chain program would compute
    /// and asserts our library produces the same result.
    #[test]
    fn test_matches_onchain_algorithm() {
        use sha3::{Digest, Keccak256};

        let authority = [42u8; 32];
        let points: u64 = 1234;

        // Manual on-chain leaf computation (from points.rs lines 192-195).
        let mut hasher = Keccak256::new();
        hasher.update(&authority);
        hasher.update(&points.to_le_bytes());
        let expected_leaf: [u8; 32] = hasher.finalize().into();

        assert_eq!(hash_leaf(&authority, points), expected_leaf);

        // Manual on-chain pair computation (from points.rs lines 204-211).
        let a = [0x01u8; 32];
        let b = [0x02u8; 32];

        let mut hasher = Keccak256::new();
        if a <= b {
            hasher.update(&a);
            hasher.update(&b);
        } else {
            hasher.update(&b);
            hasher.update(&a);
        }
        let expected_pair: [u8; 32] = hasher.finalize().into();

        assert_eq!(hash_pair(&a, &b), expected_pair);
    }

    /// Ensure tree is deterministic — same inputs always yield same root.
    #[test]
    fn test_deterministic() {
        let entries: Vec<(Pubkey, u64)> = (1..=5u8)
            .map(|i| (test_pubkey(i), i as u64 * 100))
            .collect();
        let tree1 = build_merkle_tree(&entries).unwrap();
        let tree2 = build_merkle_tree(&entries).unwrap();
        assert_eq!(tree1.root, tree2.root);
    }
}
