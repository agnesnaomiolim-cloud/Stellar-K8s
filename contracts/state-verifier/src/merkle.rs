//! Transaction inclusion proofs.
//!
//! Tree shape (RFC 6962 §2.1, with SHA-256 via the Soroban host):
//!   leaf  = H(0x00 ‖ tx_hash)
//!   node  = H(0x01 ‖ left ‖ right)
//! Leaves are the ledger's transaction hashes in application order. A node
//! without a right sibling is promoted unchanged, so proofs contain exactly
//! one sibling per level where one exists and the tree is never padded.
//!
//! Cost per level is three host calls (bytes_new_from_linear_memory,
//! compute_hash_sha256, bytes_copy_to_linear_memory); the whole proof is
//! copied into guest memory with a single call up front.

use soroban_sdk::{Bytes, Env};

use crate::xdr_parser::Hash;

/// A tree of `2^32` leaves has depth 32; proofs are at most this long.
pub const MAX_DEPTH: usize = 32;

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProofError {
    /// `index >= leaf_count`.
    IndexOutOfRange,
    /// Proof length does not match the path implied by `index` / `leaf_count`.
    BadProofLength,
}

#[inline]
fn sha256(env: &Env, data: &[u8]) -> Hash {
    env.crypto()
        .sha256(&Bytes::from_slice(env, data))
        .to_array()
}

pub fn leaf_hash(env: &Env, tx_hash: &Hash) -> Hash {
    let mut buf = [0u8; 33];
    buf[0] = LEAF_PREFIX;
    buf[1..].copy_from_slice(tx_hash);
    sha256(env, &buf)
}

/// Number of siblings an audit path for `index` in a tree of `leaf_count`
/// leaves contains.
pub fn path_len(mut index: u32, leaf_count: u32) -> usize {
    let Some(mut last) = leaf_count.checked_sub(1) else {
        return 0;
    };
    let mut n = 0;
    while last > 0 {
        if index & 1 == 1 || index < last {
            n += 1;
        }
        index >>= 1;
        last >>= 1;
    }
    n
}

/// Recomputes the root from `tx_hash`, its `index`, the tree's `leaf_count`
/// and the concatenated sibling hashes in `proof` (leaf level first).
pub fn compute_root(
    env: &Env,
    tx_hash: &Hash,
    index: u32,
    leaf_count: u32,
    proof: &Bytes,
) -> Result<Hash, ProofError> {
    if index >= leaf_count {
        return Err(ProofError::IndexOutOfRange);
    }
    let siblings = path_len(index, leaf_count);
    if proof.len() as usize != siblings * 32 {
        return Err(ProofError::BadProofLength);
    }
    let mut path = [0u8; MAX_DEPTH * 32];
    proof.copy_into_slice(&mut path[..siblings * 32]);

    let mut node = [0u8; 65];
    node[0] = NODE_PREFIX;
    let mut h = leaf_hash(env, tx_hash);
    let (mut idx, mut last, mut k) = (index, leaf_count - 1, 0usize);
    while last > 0 {
        if idx & 1 == 1 {
            node[1..33].copy_from_slice(&path[k..k + 32]);
            node[33..].copy_from_slice(&h);
            h = sha256(env, &node);
            k += 32;
        } else if idx < last {
            node[1..33].copy_from_slice(&h);
            node[33..].copy_from_slice(&path[k..k + 32]);
            h = sha256(env, &node);
            k += 32;
        } // else: rightmost node without a sibling is promoted.
        idx >>= 1;
        last >>= 1;
    }
    Ok(h)
}
