//! # Trustless Smart Contract State Verification Protocol
//!
//! Issue #318 — Soroban on-chain light-client for Stellar state verification.
//!
//! ## Overview
//!
//! This contract acts as a **native on-chain light client**.  Layer-2 rollups
//! and cross-chain bridges call it to verify that a specific piece of Stellar
//! state (a transaction, account balance snapshot, or arbitrary payload) was
//! committed to a publicly-anchored Merkle root without running a full
//! Captive Core node.
//!
//! ## Architecture
//!
//! ```text
//!  Off-chain prover                On-chain verifier
//!  ──────────────────              ────────────────────────────
//!  1. Parse ledger XDR             StateVerifier contract
//!  2. Build Merkle proof           ├─ xdr_parse  – decode ledger headers
//!  3. call submit_proof()  ──────► ├─ merkle     – SHA-256 proof engine
//!                                  └─ registry   – per-ledger root store
//! ```
//!
//! ## Key entry points
//!
//! | Function            | Description                                          |
//! |---------------------|------------------------------------------------------|
//! | `initialize`        | One-time setup, sets admin address                   |
//! | `anchor_root_raw`   | Admin: anchor a trusted Merkle root for a ledger seq |
//! | `anchor_root`       | Admin: anchor root extracted from a ledger XDR       |
//! | `submit_proof`      | Submit + verify a state inclusion proof (writes)     |
//! | `verify_proof`      | Pure verification (read-only, no state write)         |
//! | `get_anchored_root` | Read back a previously anchored root                 |
//!
//! ## Instruction-budget design
//!
//! * XDR parsing is O(1) in ledger size — only a fixed-size header subset is
//!   decoded.
//! * Proof verification is O(depth) SHA-256 calls (max 32 for a 2³² leaf tree).
//! * Registry reads/writes are keyed by `u32` ledger sequence — a single
//!   storage slot access per call.

#![no_std]
extern crate alloc;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype,
    Address, Bytes, BytesN, Env,
    Vec,
};

pub mod merkle;
pub mod xdr_parse;

use merkle::{Direction, Hash32, ProofStep};
use xdr_parse::{LedgerHeader, hash_state_payload};

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

/// Contract-level errors.
#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ContractError {
    /// Caller is not the designated administrator.
    Unauthorized           = 1,
    /// The requested ledger has no anchored Merkle root.
    RootNotFound           = 2,
    /// The Merkle proof failed mathematical verification.
    InvalidProof           = 3,
    /// Supplied XDR bytes could not be decoded.
    XdrParseError          = 4,
    /// A proof for this key at this ledger was already accepted.
    DuplicateProof         = 5,
    /// The direction byte in a proof step is not 0 or 1.
    InvalidProofDirection  = 6,
    /// The proof path length exceeds the maximum permitted depth.
    ProofTooDeep           = 7,
    /// The supplied payload exceeds the maximum allowed size.
    PayloadTooLarge        = 8,
    /// Contract was already initialised.
    AlreadyInitialized     = 9,
}

// ---------------------------------------------------------------------------
// Storage key types
// ---------------------------------------------------------------------------

/// Discriminated storage keys used by the registry.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Admin address (set at `initialize`).
    Admin,
    /// Trusted Merkle root for a given ledger sequence number.
    AnchoredRoot(u32),
    /// Proof-seen sentinel to prevent replay.
    ProofSeen(u32, BytesN<32>),
    /// Count of proofs verified per ledger.
    ProofCount(u32),
}

// ---------------------------------------------------------------------------
// Contract constants
// ---------------------------------------------------------------------------

/// Maximum Merkle proof depth (log₂ of the maximum tree size).
const MAX_PROOF_DEPTH: u32 = 32;

/// Maximum payload size in bytes.
const MAX_PAYLOAD_BYTES: u32 = 4096;

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct StateVerifier;

#[contractimpl]
impl StateVerifier {
    // -----------------------------------------------------------------------
    // Initialisation
    // -----------------------------------------------------------------------

    /// Initialise the contract, setting the admin address.
    ///
    /// Must be called exactly once.
    pub fn initialize(env: Env, admin: Address) -> Result<(), ContractError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(ContractError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Admin: anchor a trusted root from a raw XDR ledger header
    // -----------------------------------------------------------------------

    /// Anchor a trusted Merkle root extracted from a raw XDR `LedgerHeader`.
    ///
    /// The `bucket_list_hash` field of the decoded header is stored as the
    /// authoritative Merkle root for `ledger_seq`.
    ///
    /// Only the admin may call this function.
    pub fn anchor_root(
        env:               Env,
        caller:            Address,
        ledger_header_xdr: Bytes,
        ledger_seq:        u32,
    ) -> Result<BytesN<32>, ContractError> {
        Self::require_admin(&env, &caller)?;
        caller.require_auth();

        // Decode ledger header from XDR
        let xdr_bytes = ledger_header_xdr.to_alloc_vec();
        let header = LedgerHeader::from_xdr(&xdr_bytes)
            .map_err(|_| ContractError::XdrParseError)?;

        // Validate the declared sequence matches the encoded header
        if header.ledger_seq != ledger_seq {
            return Err(ContractError::XdrParseError);
        }

        // Store the bucket-list root (world-state Merkle root)
        let root = BytesN::from_array(&env, &header.bucket_list_hash);
        env.storage()
            .persistent()
            .set(&DataKey::AnchoredRoot(ledger_seq), &root);

        Ok(root)
    }

    /// Anchor a raw Merkle root directly (when only the root hash is known).
    ///
    /// Only the admin may call this function.
    pub fn anchor_root_raw(
        env:         Env,
        caller:      Address,
        ledger_seq:  u32,
        merkle_root: BytesN<32>,
    ) -> Result<(), ContractError> {
        Self::require_admin(&env, &caller)?;
        caller.require_auth();

        env.storage()
            .persistent()
            .set(&DataKey::AnchoredRoot(ledger_seq), &merkle_root);

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Proof submission (verifies + records)
    // -----------------------------------------------------------------------

    /// Submit a state inclusion proof for permanent on-chain verification.
    ///
    /// 1. Fetches the trusted root for `ledger_seq`.
    /// 2. Hashes `payload` as a leaf.
    /// 3. Walks the proof path via the Merkle engine.
    /// 4. If valid, records the proof to prevent replay and increments the
    ///    per-ledger proof counter.
    ///
    /// # Arguments
    ///
    /// * `ledger_seq`       – the ledger whose anchored root to verify against.
    /// * `payload`          – the raw state bytes whose inclusion is proven.
    /// * `proof_siblings`   – ordered sibling hashes from leaf→root.
    /// * `proof_directions` – one byte per sibling (0 = Left, 1 = Right).
    ///
    /// # Returns
    ///
    /// The leaf hash (`BytesN<32>`) on success.
    pub fn submit_proof(
        env:              Env,
        ledger_seq:       u32,
        payload:          Bytes,
        proof_siblings:   Vec<BytesN<32>>,
        proof_directions: Bytes,
    ) -> Result<BytesN<32>, ContractError> {
        // Bound checks
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(ContractError::PayloadTooLarge);
        }
        if proof_siblings.len() > MAX_PROOF_DEPTH {
            return Err(ContractError::ProofTooDeep);
        }
        if proof_siblings.len() != proof_directions.len() {
            return Err(ContractError::InvalidProof);
        }

        // Fetch trusted root
        let trusted_root_bytes: BytesN<32> = env
            .storage()
            .persistent()
            .get(&DataKey::AnchoredRoot(ledger_seq))
            .ok_or(ContractError::RootNotFound)?;
        let trusted_root = trusted_root_bytes.to_array();

        // Compute leaf hash
        let payload_slice = payload.to_alloc_vec();
        let leaf_hash: Hash32 = hash_state_payload(&payload_slice);
        let leaf_bytes = BytesN::from_array(&env, &leaf_hash);

        // Duplicate-proof guard
        let seen_key = DataKey::ProofSeen(ledger_seq, leaf_bytes.clone());
        if env.storage().persistent().has(&seen_key) {
            return Err(ContractError::DuplicateProof);
        }

        // Build proof path
        let path = Self::build_proof_path(&proof_siblings, &proof_directions)?;

        // Verify
        if !merkle::verify_proof(leaf_hash, trusted_root, &path) {
            return Err(ContractError::InvalidProof);
        }

        // Record proof as seen
        env.storage().persistent().set(&seen_key, &true);

        // Increment counter
        let count_key = DataKey::ProofCount(ledger_seq);
        let prev: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        env.storage().persistent().set(&count_key, &(prev + 1));

        Ok(leaf_bytes)
    }

    // -----------------------------------------------------------------------
    // Pure verification (read-only, no state write)
    // -----------------------------------------------------------------------

    /// Verify a Merkle inclusion proof without recording it.
    ///
    /// Suitable for dry-run calls from off-chain clients.
    pub fn verify_proof(
        env:              Env,
        ledger_seq:       u32,
        payload:          Bytes,
        proof_siblings:   Vec<BytesN<32>>,
        proof_directions: Bytes,
    ) -> Result<bool, ContractError> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(ContractError::PayloadTooLarge);
        }
        if proof_siblings.len() > MAX_PROOF_DEPTH {
            return Err(ContractError::ProofTooDeep);
        }

        let trusted_root_bytes: BytesN<32> = env
            .storage()
            .persistent()
            .get(&DataKey::AnchoredRoot(ledger_seq))
            .ok_or(ContractError::RootNotFound)?;
        let trusted_root = trusted_root_bytes.to_array();

        let payload_slice = payload.to_alloc_vec();
        let leaf_hash: Hash32 = hash_state_payload(&payload_slice);

        let path = Self::build_proof_path(&proof_siblings, &proof_directions)?;

        Ok(merkle::verify_proof(leaf_hash, trusted_root, &path))
    }

    // -----------------------------------------------------------------------
    // Registry reads
    // -----------------------------------------------------------------------

    /// Return the anchored Merkle root for a given ledger sequence, if set.
    pub fn get_anchored_root(env: Env, ledger_seq: u32) -> Option<BytesN<32>> {
        env.storage()
            .persistent()
            .get(&DataKey::AnchoredRoot(ledger_seq))
    }

    /// Return the number of proofs accepted for a given ledger.
    pub fn get_proof_count(env: Env, ledger_seq: u32) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ProofCount(ledger_seq))
            .unwrap_or(0)
    }

    /// Check whether a proof for `payload_hash` at `ledger_seq` has been accepted.
    pub fn is_proof_seen(env: Env, ledger_seq: u32, payload_hash: BytesN<32>) -> bool {
        let key = DataKey::ProofSeen(ledger_seq, payload_hash);
        env.storage().persistent().has(&key)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn require_admin(env: &Env, caller: &Address) -> Result<(), ContractError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(ContractError::Unauthorized)?;
        if *caller != admin {
            return Err(ContractError::Unauthorized);
        }
        Ok(())
    }

    /// Convert parallel SDK Vec of siblings + direction bytes into a native
    /// `alloc::vec::Vec<ProofStep>` for the Merkle engine.
    fn build_proof_path(
        siblings:   &Vec<BytesN<32>>,
        directions: &Bytes,
    ) -> Result<alloc::vec::Vec<ProofStep>, ContractError> {
        let len = siblings.len() as usize;
        let mut path: alloc::vec::Vec<ProofStep> = alloc::vec::Vec::with_capacity(len);

        for i in 0..(len as u32) {
            let sibling_bytes = siblings.get(i).unwrap();
            let dir_byte = directions.get(i).unwrap();

            let direction = Direction::from_byte(dir_byte)
                .ok_or(ContractError::InvalidProofDirection)?;

            path.push(ProofStep {
                sibling: sibling_bytes.to_array(),
                direction,
            });
        }

        Ok(path)
    }
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests — off-chain harness: generate Merkle proofs and validate them
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    use crate::merkle::{build_merkle_root, build_proof, hash_leaf, Direction as Dir};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn setup() -> (Env, StateVerifierClient<'static>) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(StateVerifier, ());
        let client = StateVerifierClient::new(&env, &id);
        (env, client)
    }

    fn h32(env: &Env, arr: Hash32) -> BytesN<32> {
        BytesN::from_array(env, &arr)
    }

    fn siblings_vec(env: &Env, siblings: &[Hash32]) -> Vec<BytesN<32>> {
        let mut v: Vec<BytesN<32>> = Vec::new(env);
        for s in siblings {
            v.push_back(h32(env, *s));
        }
        v
    }

    fn dir_bytes(env: &Env, dirs: &[Dir]) -> Bytes {
        let raw: alloc::vec::Vec<u8> = dirs.iter().map(|d| *d as u8).collect();
        Bytes::from_slice(env, &raw)
    }

    // -----------------------------------------------------------------------
    // Test: double-initialize is rejected
    // -----------------------------------------------------------------------

    #[test]
    fn test_double_initialize_rejected() {
        let (env, client) = setup();
        let admin = Address::generate(&env);

        // First init succeeds (direct call panics on error)
        client.initialize(&admin);

        // Second init must panic (AlreadyInitialized)
        let result = client.try_initialize(&admin);
        assert!(result.is_err(), "second initialize must be rejected");
    }

    // -----------------------------------------------------------------------
    // Test: happy-path — 4-leaf tree, verify all leaves, submit first
    // -----------------------------------------------------------------------

    #[test]
    fn test_submit_proof_four_leaves() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        // Build 4-leaf Merkle tree
        let payloads: alloc::vec::Vec<&[u8]> = alloc::vec![
            b"account:GABC:balance:10000000" as &[u8],
            b"account:GDEF:balance:20000000",
            b"account:GHIJ:balance:30000000",
            b"account:GKLM:balance:40000000",
        ];
        let leaf_hashes: alloc::vec::Vec<Hash32> =
            payloads.iter().map(|p| hash_leaf(p)).collect();
        let root = build_merkle_root(&leaf_hashes).unwrap();

        let ledger_seq: u32 = 1000;
        client.anchor_root_raw(&admin, &ledger_seq, &h32(&env, root));

        // Confirm stored root
        assert_eq!(client.get_anchored_root(&ledger_seq), Some(h32(&env, root)));

        // Verify each leaf with read-only call
        for idx in 0..4usize {
            let (proof_root, proof_steps) = build_proof(&leaf_hashes, idx).unwrap();
            assert_eq!(proof_root, root);

            let sibs: alloc::vec::Vec<Hash32> = proof_steps.iter().map(|s| s.sibling).collect();
            let dirs: alloc::vec::Vec<Dir> = proof_steps.iter().map(|s| s.direction).collect();

            let payload_b = Bytes::from_slice(&env, payloads[idx]);
            let ok = client.verify_proof(
                &ledger_seq,
                &payload_b,
                &siblings_vec(&env, &sibs),
                &dir_bytes(&env, &dirs),
            );
            assert!(ok, "verify_proof failed for leaf {}", idx);
        }

        // Submit leaf 0 (state-writing call)
        let (_, steps0) = build_proof(&leaf_hashes, 0).unwrap();
        let sibs0: alloc::vec::Vec<Hash32> = steps0.iter().map(|s| s.sibling).collect();
        let dirs0: alloc::vec::Vec<Dir> = steps0.iter().map(|s| s.direction).collect();

        let leaf_b = client.submit_proof(
            &ledger_seq,
            &Bytes::from_slice(&env, payloads[0]),
            &siblings_vec(&env, &sibs0),
            &dir_bytes(&env, &dirs0),
        );
        assert_eq!(leaf_b, h32(&env, leaf_hashes[0]));
        assert_eq!(client.get_proof_count(&ledger_seq), 1);
    }

    // -----------------------------------------------------------------------
    // Test: single-leaf tree (leaf IS the root, empty proof path)
    // -----------------------------------------------------------------------

    #[test]
    fn test_single_leaf_tree() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let payload: &[u8] = b"solo:state:payload";
        let leaf_hash = hash_leaf(payload);
        let root = build_merkle_root(&[leaf_hash]).unwrap();
        // Single leaf: root == leaf
        assert_eq!(root, leaf_hash);

        client.anchor_root_raw(&admin, &1u32, &h32(&env, root));

        // Empty proof path
        let ok = client.verify_proof(
            &1u32,
            &Bytes::from_slice(&env, payload),
            &Vec::new(&env),
            &Bytes::from_slice(&env, &[]),
        );
        assert!(ok, "single-leaf empty-path proof must succeed");
    }

    // -----------------------------------------------------------------------
    // Test: duplicate proof is rejected
    // -----------------------------------------------------------------------

    #[test]
    fn test_duplicate_proof_rejected() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let payloads: alloc::vec::Vec<&[u8]> = alloc::vec![b"leaf0" as &[u8], b"leaf1"];
        let leaf_hashes: alloc::vec::Vec<Hash32> =
            payloads.iter().map(|p| hash_leaf(p)).collect();
        let root = build_merkle_root(&leaf_hashes).unwrap();
        client.anchor_root_raw(&admin, &42u32, &h32(&env, root));

        let (_, steps) = build_proof(&leaf_hashes, 0).unwrap();
        let sibs: alloc::vec::Vec<Hash32> = steps.iter().map(|s| s.sibling).collect();
        let dirs: alloc::vec::Vec<Dir> = steps.iter().map(|s| s.direction).collect();
        let payload_b = Bytes::from_slice(&env, b"leaf0");
        let sibs_v = siblings_vec(&env, &sibs);
        let dirs_v = dir_bytes(&env, &dirs);

        // First submit succeeds
        client.submit_proof(&42u32, &payload_b, &sibs_v, &dirs_v);

        // Second submit must fail
        let err = client.try_submit_proof(&42u32, &payload_b, &sibs_v, &dirs_v);
        assert!(err.is_err(), "duplicate proof must be rejected");
    }

    // -----------------------------------------------------------------------
    // Test: wrong root → verify_proof returns false
    // -----------------------------------------------------------------------

    #[test]
    fn test_wrong_root_returns_false() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        // Anchor a zeroed root (not the real tree root)
        client.anchor_root_raw(&admin, &99u32, &h32(&env, [0u8; 32]));

        let leaf_hashes: alloc::vec::Vec<Hash32> =
            [b"x" as &[u8], b"y"].iter().map(|p| hash_leaf(p)).collect();
        let (_, steps) = build_proof(&leaf_hashes, 0).unwrap();
        let sibs: alloc::vec::Vec<Hash32> = steps.iter().map(|s| s.sibling).collect();
        let dirs: alloc::vec::Vec<Dir> = steps.iter().map(|s| s.direction).collect();

        let ok = client.verify_proof(
            &99u32,
            &Bytes::from_slice(&env, b"x"),
            &siblings_vec(&env, &sibs),
            &dir_bytes(&env, &dirs),
        );
        assert!(!ok, "zeroed root must not verify");
    }

    // -----------------------------------------------------------------------
    // Test: RootNotFound when ledger not anchored
    // -----------------------------------------------------------------------

    #[test]
    fn test_root_not_found() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        // Ledger 777 was never anchored → must return error
        let err = client.try_verify_proof(
            &777u32,
            &Bytes::from_slice(&env, b"data"),
            &Vec::new(&env),
            &Bytes::from_slice(&env, &[]),
        );
        assert!(err.is_err(), "unanchored ledger must return error");
    }

    // -----------------------------------------------------------------------
    // Test: unauthorized anchor is rejected
    // -----------------------------------------------------------------------

    #[test]
    fn test_unauthorized_anchor_rejected() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        let attacker = Address::generate(&env);
        client.initialize(&admin);

        let err = client.try_anchor_root_raw(&attacker, &1u32, &h32(&env, [0u8; 32]));
        assert!(err.is_err(), "non-admin anchor must be rejected");
    }

    // -----------------------------------------------------------------------
    // Test: is_proof_seen reflects submitted proofs
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_proof_seen() {
        let (env, client) = setup();
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let payload: &[u8] = b"seen:test:payload";
        let leaf_hash = hash_leaf(payload);
        let root = build_merkle_root(&[leaf_hash]).unwrap();
        client.anchor_root_raw(&admin, &5u32, &h32(&env, root));

        let leaf_bytes = h32(&env, leaf_hash);
        assert!(!client.is_proof_seen(&5u32, &leaf_bytes));

        client.submit_proof(
            &5u32,
            &Bytes::from_slice(&env, payload),
            &Vec::new(&env),
            &Bytes::from_slice(&env, &[]),
        );

        assert!(client.is_proof_seen(&5u32, &leaf_bytes));
    }
}
