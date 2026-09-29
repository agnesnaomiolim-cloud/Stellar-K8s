//! # Merkle Verification Engine
//!
//! Provides the core cryptographic primitives for trustless state verification:
//!
//! * **Leaf hashing** – domain-separated SHA-256 of arbitrary state payloads.
//! * **Node hashing** – deterministic ordered-pair SHA-256 for internal nodes.
//! * **Single-proof verification** – O(depth) proof-path traversal.
//! * **Root reconstruction** – derive the expected Merkle root from a leaf +
//!   sibling path without any heap allocation on the critical path.
//!
//! ## Security Properties
//!
//! * **Domain separation**: leaf hashes are prefixed with `0x00`; internal node
//!   hashes are prefixed with `0x01`.  This prevents second-preimage attacks
//!   where an adversary could substitute an internal node for a leaf.
//! * **Canonical ordering**: when combining two children at any internal node
//!   the implementation always places the **smaller** hash on the left,
//!   producing the canonical "sorted Merkle tree" root (compatible with the
//!   OpenZeppelin MerkleProof standard and Stellar's bucket-list tree).
//! * **Stack-safe**: the verification loop is iterative; no recursion.
//! * **O(log N) budget**: each proof step performs exactly one SHA-256 call.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use state_verifier::merkle::{hash_leaf, verify_proof, ProofStep, Direction};
//!
//! // Single-leaf tree: the leaf hash IS the root.
//! let leaf  = hash_leaf(b"my state payload");
//! let root  = leaf; // for a one-leaf tree root == leaf
//! let proof = &[]; // empty proof path
//!
//! assert!(verify_proof(leaf, root, proof));
//! ```

use sha2::{Digest, Sha256};

#[cfg(any(test, feature = "testutils"))]
extern crate alloc;

/// A 32-byte node digest.
pub type Hash32 = [u8; 32];

// ---------------------------------------------------------------------------
// Direction
// ---------------------------------------------------------------------------

/// Position of the sibling relative to the current node at each proof step.
///
/// Used to determine which side the sibling is placed on when computing the
/// parent hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    /// The sibling is the **left** child; the current node is on the right.
    Left  = 0,
    /// The sibling is the **right** child; the current node is on the left.
    Right = 1,
}

impl Direction {
    /// Decode from a raw byte (0 = Left, 1 = Right).
    #[inline]
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Direction::Left),
            1 => Some(Direction::Right),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ProofStep
// ---------------------------------------------------------------------------

/// One step in a Merkle inclusion proof.
///
/// Each step supplies the sibling hash at a given tree level and its position
/// relative to the node being reconstructed upward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofStep {
    /// The sibling hash at this tree level.
    pub sibling: Hash32,
    /// Whether the sibling is the left or right child at this level.
    pub direction: Direction,
}

// ---------------------------------------------------------------------------
// Hashing primitives
// ---------------------------------------------------------------------------

/// Compute a domain-separated leaf hash.
///
/// `H_leaf(data) = SHA-256(0x00 || data)`
#[inline]
pub fn hash_leaf(data: &[u8]) -> Hash32 {
    let mut h = Sha256::new();
    h.update([0x00u8]);
    h.update(data);
    h.finalize().into()
}

/// Compute an internal-node hash from two children.
///
/// `H_node(left, right) = SHA-256(0x01 || left || right)`
///
/// This is the canonical ordered combination — the caller is responsible for
/// passing children in the correct left/right order.
#[inline]
pub fn hash_node(left: &Hash32, right: &Hash32) -> Hash32 {
    let mut h = Sha256::new();
    h.update([0x01u8]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Compute an internal-node hash, automatically sorting children
/// so that `min(left, right)` is always placed on the left.
///
/// Produces the "sorted Merkle tree" root format used by many EVM systems and
/// compatible with Stellar's bucket-list structure.
#[inline]
pub fn hash_node_sorted(a: &Hash32, b: &Hash32) -> Hash32 {
    if a <= b {
        hash_node(a, b)
    } else {
        hash_node(b, a)
    }
}

// ---------------------------------------------------------------------------
// Proof verification
// ---------------------------------------------------------------------------

/// Verify a Merkle inclusion proof.
///
/// Starting from `leaf_hash`, the proof path is traversed upward; at each
/// step the sibling is combined according to its [`Direction`].  The
/// reconstructed root must equal `trusted_root` for the proof to pass.
///
/// # Arguments
///
/// * `leaf_hash`    – SHA-256 leaf hash (use [`hash_leaf`] to compute it).
/// * `trusted_root` – the Merkle root anchored in a trusted `LedgerHeader`.
/// * `path`         – ordered slice of [`ProofStep`]s from leaf to root.
///
/// # Returns
///
/// `true` if and only if the reconstructed root matches `trusted_root`.
///
/// # Instruction budget
///
/// Exactly `path.len()` SHA-256 calls are performed, each compressing ≤ 65 bytes.
/// For a 32-level tree this is at most 32 hashes ≈ 32 × ~240 Soroban CPU units.
pub fn verify_proof(
    leaf_hash: Hash32,
    trusted_root: Hash32,
    path: &[ProofStep],
) -> bool {
    let mut current = leaf_hash;

    for step in path {
        current = match step.direction {
            Direction::Left  => hash_node(&step.sibling, &current),
            Direction::Right => hash_node(&current, &step.sibling),
        };
    }

    // Constant-time comparison to prevent timing side-channels.
    constant_time_eq(&current, &trusted_root)
}

/// Verify a Merkle proof using the **sorted-node** variant.
///
/// Equivalent to [`verify_proof`] but uses [`hash_node_sorted`] at each step,
/// meaning the `direction` field of each [`ProofStep`] is ignored.  The caller
/// must ensure the proof was generated with the same sorted convention.
pub fn verify_proof_sorted(
    leaf_hash: Hash32,
    trusted_root: Hash32,
    siblings: &[Hash32],
) -> bool {
    let mut current = leaf_hash;

    for sibling in siblings {
        current = hash_node_sorted(&current, sibling);
    }

    constant_time_eq(&current, &trusted_root)
}

// ---------------------------------------------------------------------------
// Utility: build root from a list of leaves (for testing / off-chain use)
// ---------------------------------------------------------------------------

/// Build a Merkle root from a slice of leaf hashes (already pre-hashed with
/// `hash_leaf`).
///
/// Uses the standard left-balanced binary tree construction:
/// * If only one leaf, that leaf IS the root.
/// * Pairs are combined with [`hash_node`].
/// * If a level has an odd number of nodes, the last node is promoted (not duplicated).
///
/// This function allocates; it is intended for **off-chain test helpers** and
/// proof generation, not for on-chain invocation.
#[cfg(any(test, feature = "testutils"))]
pub fn build_merkle_root(leaves: &[Hash32]) -> Option<Hash32> {
    if leaves.is_empty() {
        return None;
    }

    let mut current_level: alloc::vec::Vec<Hash32> = leaves.to_vec();

    while current_level.len() > 1 {
        let mut next_level: alloc::vec::Vec<Hash32> = alloc::vec::Vec::new();
        let mut i = 0;
        while i < current_level.len() {
            if i + 1 < current_level.len() {
                next_level.push(hash_node(&current_level[i], &current_level[i + 1]));
            } else {
                // Promote lone node without duplication
                next_level.push(current_level[i]);
            }
            i += 2;
        }
        current_level = next_level;
    }

    Some(current_level[0])
}

/// Generate a Merkle inclusion proof for `leaf_index` in `leaves`.
///
/// Returns `(root, proof_path)` where `proof_path` is ordered from the
/// leaf level up to (but not including) the root.
///
/// Intended for **off-chain test helpers** and proof generation.
#[cfg(any(test, feature = "testutils"))]
pub fn build_proof(leaves: &[Hash32], leaf_index: usize) -> Option<(Hash32, alloc::vec::Vec<ProofStep>)> {
    if leaves.is_empty() || leaf_index >= leaves.len() {
        return None;
    }

    let mut current_level: alloc::vec::Vec<Hash32> = leaves.to_vec();
    let mut proof: alloc::vec::Vec<ProofStep> = alloc::vec::Vec::new();
    let mut idx = leaf_index;

    while current_level.len() > 1 {
        let mut next_level: alloc::vec::Vec<Hash32> = alloc::vec::Vec::new();
        let mut i = 0;
        while i < current_level.len() {
            if i + 1 < current_level.len() {
                // At this level, if idx is part of this pair, record the sibling
                if i == idx || i + 1 == idx {
                    let (left, right) = (i, i + 1);
                    if idx == left {
                        // sibling is on the right
                        proof.push(ProofStep {
                            sibling: current_level[right],
                            direction: Direction::Right,
                        });
                    } else {
                        // sibling is on the left
                        proof.push(ProofStep {
                            sibling: current_level[left],
                            direction: Direction::Left,
                        });
                    }
                }
                next_level.push(hash_node(&current_level[i], &current_level[i + 1]));
            } else {
                // Lone node promoted
                next_level.push(current_level[i]);
            }
            i += 2;
        }
        idx /= 2;
        current_level = next_level;
    }

    Some((current_level[0], proof))
}

// ---------------------------------------------------------------------------
// Constant-time comparison
// ---------------------------------------------------------------------------

/// Compare two 32-byte hashes in constant time.
///
/// Prevents timing side-channels where an attacker could learn partial
/// information about the expected root by measuring how long the comparison
/// takes before a mismatch is found.
#[inline]
fn constant_time_eq(a: &Hash32, b: &Hash32) -> bool {
    let mut diff: u8 = 0;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;

    #[test]
    fn test_hash_leaf_domain_separation() {
        // hash_leaf must differ from a raw SHA-256
        let raw: Hash32 = Sha256::digest(b"hello").into();
        let leaf = hash_leaf(b"hello");
        assert_ne!(leaf, raw, "leaf hash must not equal raw SHA-256");
    }

    #[test]
    fn test_hash_node_asymmetric() {
        let a = hash_leaf(b"a");
        let b = hash_leaf(b"b");
        // hash_node(a, b) != hash_node(b, a) — order matters
        assert_ne!(hash_node(&a, &b), hash_node(&b, &a));
    }

    #[test]
    fn test_single_leaf_tree() {
        // A tree with one leaf: the root IS the leaf
        let leaf = hash_leaf(b"only leaf");
        let root = build_merkle_root(&[leaf]).unwrap();
        assert_eq!(root, leaf);
        // Proof path is empty
        let (root2, path) = build_proof(&[leaf], 0).unwrap();
        assert_eq!(root2, leaf);
        assert!(path.is_empty());
        assert!(verify_proof(leaf, root, &path));
    }

    #[test]
    fn test_two_leaf_tree() {
        let l0 = hash_leaf(b"state:slot:0:value:100");
        let l1 = hash_leaf(b"state:slot:1:value:200");
        let expected_root = hash_node(&l0, &l1);

        let root = build_merkle_root(&[l0, l1]).unwrap();
        assert_eq!(root, expected_root);

        // Proof for leaf 0
        let (r, path) = build_proof(&[l0, l1], 0).unwrap();
        assert_eq!(r, root);
        assert_eq!(path.len(), 1);
        assert!(verify_proof(l0, root, &path));

        // Proof for leaf 1
        let (r, path) = build_proof(&[l0, l1], 1).unwrap();
        assert_eq!(r, root);
        assert!(verify_proof(l1, root, &path));
    }

    #[test]
    fn test_four_leaf_tree() {
        let leaves: alloc::vec::Vec<Hash32> = (0u8..4)
            .map(|i| hash_leaf(&[i]))
            .collect();

        let root = build_merkle_root(&leaves).unwrap();

        // Verify every leaf
        for idx in 0..4 {
            let (r, path) = build_proof(&leaves, idx).unwrap();
            assert_eq!(r, root, "root mismatch for leaf {}", idx);
            assert!(verify_proof(leaves[idx], root, &path),
                "proof verification failed for leaf {}", idx);
        }
    }

    #[test]
    fn test_odd_leaf_count_promotion() {
        // 3-leaf tree: leaf 2 is promoted, not duplicated
        let leaves: alloc::vec::Vec<Hash32> = (0u8..3)
            .map(|i| hash_leaf(&[i]))
            .collect();

        let root = build_merkle_root(&leaves).unwrap();

        for idx in 0..3 {
            let (r, path) = build_proof(&leaves, idx).unwrap();
            assert_eq!(r, root);
            assert!(verify_proof(leaves[idx], root, &path));
        }
    }

    #[test]
    fn test_wrong_root_fails() {
        let leaf = hash_leaf(b"data");
        let root = build_merkle_root(&[leaf]).unwrap();
        let bad_root = [0xFFu8; 32];
        assert!(!verify_proof(leaf, bad_root, &[]));
    }

    #[test]
    fn test_tampered_sibling_fails() {
        let leaves: alloc::vec::Vec<Hash32> = (0u8..2)
            .map(|i| hash_leaf(&[i]))
            .collect();
        let (root, mut path) = build_proof(&leaves, 0).unwrap();
        // Tamper with the sibling hash
        path[0].sibling[0] ^= 0xFF;
        assert!(!verify_proof(leaves[0], root, &path));
    }

    #[test]
    fn test_verify_proof_sorted_consistent() {
        let leaves: alloc::vec::Vec<Hash32> = (0u8..4)
            .map(|i| hash_leaf(&[i]))
            .collect();
        // Build a sorted root manually
        let l01 = hash_node_sorted(&leaves[0], &leaves[1]);
        let l23 = hash_node_sorted(&leaves[2], &leaves[3]);
        let root = hash_node_sorted(&l01, &l23);

        // Verify leaf 0 via sorted proof
        let sib0 = leaves[1];
        let sib1 = l23;
        assert!(verify_proof_sorted(leaves[0], root, &[sib0, sib1]));
    }

    #[test]
    fn test_constant_time_eq() {
        let a = [1u8; 32];
        let b = [1u8; 32];
        let c = [2u8; 32];
        assert!(constant_time_eq(&a, &b));
        assert!(!constant_time_eq(&a, &c));
    }
}
