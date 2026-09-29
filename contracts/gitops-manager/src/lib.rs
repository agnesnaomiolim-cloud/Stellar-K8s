// src/lib.rs
//! GitOps Manager contract
//!
//! This contract allows on‑chain governance proposals to be submitted and
//! approved by a configurable set of signers. Once a proposal reaches the
//! required multi‑signature threshold, an event is emitted that contains a
//! lightweight payload (IPFS hash or Git commit SHA) which can be consumed by
//! external GitOps pipelines.

use soroban_sdk::{
    contractimpl, contracttype, Address, Bytes, BytesN, Env, Symbol, Vec, map, log,
};

mod events;
use events::{EVENT_PROPOSAL_APPROVED, EVENT_PROPOSAL_SUBMITTED};

/// Storage keys used by the contract.
#[contracttype]
enum DataKey {
    // Counter for the next proposal identifier.
    ProposalCounter,
    // Configuration of authorized signers and the threshold.
    Signers,
    Threshold,
    // Individual proposals stored by id.
    Proposal(u64),
}

/// A governance proposal.
#[contracttype]
pub struct Proposal {
    /// Hash of the payload (e.g., IPFS CID or Git SHA).
    pub payload_hash: Bytes,
    /// Signatures that have been collected.
    pub signatures: Vec<(Address, BytesN<64>)>,
    /// Whether the proposal has already been approved (event emitted).
    pub approved: bool,
}

/// Public contract interface.
pub struct GitOpsManager;

#[contractimpl]
impl GitOpsManager {
    /// Initialize the contract with a set of authorized signers and a threshold.
    ///
    /// * `signers` – a vector of addresses that are allowed to sign.
    /// * `threshold` – number of distinct signatures required to approve.
    pub fn initialize(env: Env, signers: Vec<Address>, threshold: u32) {
        // Store configuration.
        env.storage().persistent().set(&DataKey::Signers, &signers);
        env.storage().persistent().set(&DataKey::Threshold, &threshold);
        // Initialise proposal counter.
        env.storage().persistent().set(&DataKey::ProposalCounter, &0u64);
        log!(&env, "GitOpsManager initialized with {} signers, threshold {}", signers.len(), threshold);
    }

    /// Submit a new proposal containing a payload hash.
    /// Returns the generated proposal identifier.
    pub fn submit_proposal(env: Env, payload_hash: Bytes) -> u64 {
        // Increment counter.
        let mut counter: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::ProposalCounter)
            .unwrap_or(0);
        counter += 1;
        env.storage().persistent().set(&DataKey::ProposalCounter, &counter);

        // Store the proposal.
        let proposal = Proposal {
            payload_hash: payload_hash.clone(),
            signatures: Vec::new(&env),
            approved: false,
        };
        env.storage().persistent().set(&DataKey::Proposal(counter), &proposal);

        // Emit a submission event (lightweight).
        env.events().publish(
            (Symbol::new(&env, EVENT_PROPOSAL_SUBMITTED),),
            (counter, payload_hash),
        );
        log!(&env, "Proposal {} submitted", counter);
        counter
    }

    /// Add a signature for a proposal. Returns true if the proposal becomes approved.
    pub fn approve(
        env: Env,
        proposal_id: u64,
        signer: Address,
        signature: BytesN<64>,
    ) -> bool {
        // Verify signer is authorized.
        let signers: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Signers)
            .expect("signers not initialized");
        if !signers.iter().any(|a| a == &signer) {
            panic!("unauthorized signer");
        }

        // Retrieve and update the proposal.
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .expect("proposal not found");

        // Prevent duplicate signatures from same signer.
        if proposal.signatures.iter().any(|(s, _)| s == &signer) {
            panic!("signer already approved");
        }

        // Verify the Ed25519 signature against the stored payload hash.
        // NOTE: This is a simplistic verification – in a real contract you would
        // need the public key associated with `signer`. For demonstration we
        // assume the signer address encodes the public key and use the SDK helper.
        let pk_bytes: BytesN<32> = signer.public_key(); // placeholder method
        // The SDK provides `ed25519_verify` – we use it via Env.
        // If verification fails it will panic.
        env.crypto().ed25519_verify(
            &pk_bytes,
            &proposal.payload_hash,
            &signature,
        );

        // Record the signature.
        proposal.signatures.push_back((signer.clone(), signature));

        // Check if we have reached the threshold.
        let threshold: u32 = env.storage().persistent().get(&DataKey::Threshold).unwrap();
        if (proposal.signatures.len() as u32) >= threshold && !proposal.approved {
            // Mark approved and emit the main GitOps event.
            proposal.approved = true;
            env.events().publish(
                (Symbol::new(&env, EVENT_PROPOSAL_APPROVED),),
                (
                    proposal_id,
                    proposal.payload_hash.clone(),
                    // Emit the list of signers for auditability.
                    proposal.signatures.iter().map(|(a, _)| a.clone()).collect::<Vec<Address>>(),
                ),
            );
            log!(&env, "Proposal {} approved and event emitted", proposal_id);
        }

        // Persist the updated proposal.
        env.storage().persistent().set(&DataKey::Proposal(proposal_id), &proposal);
        proposal.approved
    }
}
