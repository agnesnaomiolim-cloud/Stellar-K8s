//! Stellar state verification & snapshot oracle.
//!
//! Authorized relayers checkpoint Stellar ledger headers. The contract parses
//! each header from XDR inside WASM, computes the ledger hash itself
//! (`SHA-256(header XDR)`), and hash-chains it to neighbouring checkpoints
//! through `previousLedgerHash`. Each checkpoint also carries the root of a
//! Merkle tree over the ledger's transaction hashes. Anyone can then prove
//! that a transaction was applied in a checkpointed ledger, either from its
//! hash or from its full XDR envelope, which is decoded and hashed on-chain.
//!
//! Stellar's own `txSetHash` is a flat SHA-256 over the entire transaction
//! set, so a succinct proof cannot target it directly. The transaction root is
//! therefore attested by the relayer quorum together with the header it
//! belongs to. See README "Trust model".

#![no_std]

pub mod merkle;
pub mod xdr_parser;

#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Bytes, BytesN, Env,
    Vec,
};

use xdr_parser::{parse_envelope_head, parse_ledger_header, parse_signatures, MAX_SIGNATURES_LEN};

/// Largest possible `LedgerHeader` XDR (six maximal upgrades + signed value).
pub const MAX_HEADER_LEN: usize = 1_536;
/// Envelope bytes decoded for the transaction head (the head of any valid
/// envelope, including a fee bump with maximal preconditions, fits).
const ENVELOPE_HEAD_WINDOW: usize = 1_024;
pub const MAX_RELAYERS: u32 = 20;

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const PERSISTENT_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const PERSISTENT_THRESHOLD: u32 = PERSISTENT_BUMP - 7 * DAY_IN_LEDGERS;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidRelayerSet = 3,
    InsufficientSignatures = 4,
    DuplicateSigner = 5,
    NotRelayer = 6,
    /// Header or envelope is not valid XDR (or exceeds size limits).
    MalformedXdr = 7,
    /// A different header is already checkpointed for this ledger.
    CheckpointConflict = 8,
    /// `previousLedgerHash` does not link to an adjacent checkpoint.
    ChainMismatch = 9,
    UnknownLedger = 10,
    /// Inclusion proof does not reproduce the checkpointed root.
    InvalidProof = 11,
    /// `signatures_offset` does not split the envelope into tx + signatures.
    BadSignaturesOffset = 12,
    /// `tx_count` / `tx_root` are inconsistent (an empty ledger has a zero root).
    InvalidTxRoot = 13,
}

/// A verified ledger header plus its transaction commitment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    pub ledger_seq: u32,
    /// `SHA-256(LedgerHeader XDR)`, computed on-chain.
    pub ledger_hash: BytesN<32>,
    pub previous_ledger_hash: BytesN<32>,
    pub ledger_version: u32,
    pub close_time: u64,
    pub tx_set_hash: BytesN<32>,
    pub tx_set_result_hash: BytesN<32>,
    pub bucket_list_hash: BytesN<32>,
    /// Merkle root over the ledger's transaction hashes (application order).
    pub tx_root: BytesN<32>,
    pub tx_count: u32,
}

/// Result of a successful envelope verification.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedTx {
    pub ledger_seq: u32,
    pub ledger_hash: BytesN<32>,
    pub close_time: u64,
    pub tx_hash: BytesN<32>,
    /// 2 = `ENVELOPE_TYPE_TX`, 5 = `ENVELOPE_TYPE_TX_FEE_BUMP`.
    pub envelope_type: u32,
    /// Ed25519 key of the transaction source account.
    pub source_account: BytesN<32>,
    /// Ed25519 key of the fee-bump fee source, if any.
    pub fee_source: Option<BytesN<32>>,
    pub fee: i64,
    pub seq_num: i64,
    pub op_count: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
enum DataKey {
    Admin,
    NetworkId,
    Relayers,
    Threshold,
    Latest,
    Checkpoint(u32),
}

#[contract]
pub struct StateVerifier;

#[contractimpl]
impl StateVerifier {
    /// `network_id` is `SHA-256(network passphrase)`; it is part of every
    /// transaction hash. `relayers`/`threshold` form the checkpointing quorum.
    pub fn initialize(
        env: Env,
        admin: Address,
        network_id: BytesN<32>,
        relayers: Vec<Address>,
        threshold: u32,
    ) -> Result<(), Error> {
        let s = env.storage().instance();
        if s.has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        validate_relayer_set(&relayers, threshold)?;
        s.set(&DataKey::Admin, &admin);
        s.set(&DataKey::NetworkId, &network_id);
        s.set(&DataKey::Relayers, &relayers);
        s.set(&DataKey::Threshold, &threshold);
        bump_instance(&env);
        Ok(())
    }

    /// Replaces the relayer set. Admin only.
    pub fn set_relayers(env: Env, relayers: Vec<Address>, threshold: u32) -> Result<(), Error> {
        get::<Address>(&env, &DataKey::Admin)?.require_auth();
        validate_relayer_set(&relayers, threshold)?;
        env.storage().instance().set(&DataKey::Relayers, &relayers);
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &threshold);
        bump_instance(&env);
        env.events()
            .publish((symbol_short!("relayers"),), (relayers, threshold));
        Ok(())
    }

    /// Checkpoints a ledger header. `signers` must be a relayer quorum, each
    /// authorizing this exact call. The header is decoded and hashed
    /// on-chain; if the previous or next ledger is already checkpointed, the
    /// `previousLedgerHash` links must match. Re-submitting an identical
    /// checkpoint is a no-op. Returns the ledger hash.
    pub fn submit_checkpoint(
        env: Env,
        signers: Vec<Address>,
        header_xdr: Bytes,
        tx_root: BytesN<32>,
        tx_count: u32,
    ) -> Result<BytesN<32>, Error> {
        require_relayer_quorum(&env, &signers)?;

        let len = header_xdr.len() as usize;
        if len > MAX_HEADER_LEN {
            return Err(Error::MalformedXdr);
        }
        let mut buf = [0u8; MAX_HEADER_LEN];
        header_xdr.copy_into_slice(&mut buf[..len]);
        let header = parse_ledger_header(&buf[..len]).map_err(|_| Error::MalformedXdr)?;

        let zero = BytesN::from_array(&env, &[0u8; 32]);
        if (tx_count == 0) != (tx_root == zero) {
            return Err(Error::InvalidTxRoot);
        }

        let ledger_hash = env.crypto().sha256(&header_xdr).to_bytes();
        let seq = header.ledger_seq;
        let checkpoint = Checkpoint {
            ledger_seq: seq,
            ledger_hash: ledger_hash.clone(),
            previous_ledger_hash: BytesN::from_array(&env, &header.previous_ledger_hash),
            ledger_version: header.ledger_version,
            close_time: header.close_time,
            tx_set_hash: BytesN::from_array(&env, &header.tx_set_hash),
            tx_set_result_hash: BytesN::from_array(&env, &header.tx_set_result_hash),
            bucket_list_hash: BytesN::from_array(&env, &header.bucket_list_hash),
            tx_root,
            tx_count,
        };

        if let Some(existing) = checkpoint_of(&env, seq) {
            return if existing == checkpoint {
                Ok(ledger_hash)
            } else {
                Err(Error::CheckpointConflict)
            };
        }
        if let Some(prev) = seq.checked_sub(1).and_then(|p| checkpoint_of(&env, p)) {
            if prev.ledger_hash != checkpoint.previous_ledger_hash {
                return Err(Error::ChainMismatch);
            }
        }
        if let Some(next) = seq.checked_add(1).and_then(|n| checkpoint_of(&env, n)) {
            if next.previous_ledger_hash != ledger_hash {
                return Err(Error::ChainMismatch);
            }
        }

        let key = DataKey::Checkpoint(seq);
        env.storage().persistent().set(&key, &checkpoint);
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_THRESHOLD, PERSISTENT_BUMP);
        let latest: u32 = env.storage().instance().get(&DataKey::Latest).unwrap_or(0);
        if seq > latest {
            env.storage().instance().set(&DataKey::Latest, &seq);
        }
        bump_instance(&env);
        env.events().publish(
            (symbol_short!("checkpnt"), seq),
            (ledger_hash.clone(), checkpoint.tx_root, tx_count),
        );
        Ok(ledger_hash)
    }

    /// Proves that `tx_hash` was applied in checkpointed ledger `ledger_seq`.
    /// `proof` is the concatenated sibling hashes (leaf level first).
    /// Returns the ledger hash.
    pub fn verify_tx_hash(
        env: Env,
        ledger_seq: u32,
        tx_hash: BytesN<32>,
        index: u32,
        proof: Bytes,
    ) -> Result<BytesN<32>, Error> {
        let cp = checkpoint_of(&env, ledger_seq).ok_or(Error::UnknownLedger)?;
        check_inclusion(&env, &cp, &tx_hash.to_array(), index, &proof)?;
        Ok(cp.ledger_hash)
    }

    /// Decodes a `TransactionEnvelope`, computes its transaction hash
    /// on-chain (`SHA-256(networkId ‖ envelopeType ‖ tx)`), and proves its
    /// inclusion in checkpointed ledger `ledger_seq`.
    ///
    /// `signatures_offset` is the byte offset of the envelope's trailing
    /// `signatures<20>` array. It is validated strictly: the tail must decode
    /// as exactly one signature array and the decoded head must lie inside the
    /// hashed transaction bytes, so the reported fields are covered by the
    /// proven hash.
    pub fn verify_transaction(
        env: Env,
        ledger_seq: u32,
        envelope_xdr: Bytes,
        signatures_offset: u32,
        index: u32,
        proof: Bytes,
    ) -> Result<VerifiedTx, Error> {
        let cp = checkpoint_of(&env, ledger_seq).ok_or(Error::UnknownLedger)?;
        let len = envelope_xdr.len();
        let split = signatures_offset;
        if split <= 4 || !split.is_multiple_of(4) || split >= len {
            return Err(Error::BadSignaturesOffset);
        }
        let tail_len = (len - split) as usize;
        if tail_len > MAX_SIGNATURES_LEN {
            return Err(Error::BadSignaturesOffset);
        }
        let mut tail = [0u8; MAX_SIGNATURES_LEN];
        envelope_xdr
            .slice(split..len)
            .copy_into_slice(&mut tail[..tail_len]);
        parse_signatures(&tail[..tail_len]).map_err(|_| Error::BadSignaturesOffset)?;

        let window = (len as usize).min(ENVELOPE_HEAD_WINDOW);
        let mut head_buf = [0u8; ENVELOPE_HEAD_WINDOW];
        envelope_xdr
            .slice(0..window as u32)
            .copy_into_slice(&mut head_buf[..window]);
        let head = parse_envelope_head(&head_buf[..window]).map_err(|_| Error::MalformedXdr)?;
        if head.end > split as usize {
            return Err(Error::BadSignaturesOffset);
        }

        // TransactionSignaturePayload = networkId ‖ taggedTransaction, where
        // the tag is the envelope discriminant already at bytes 0..4.
        let network_id: BytesN<32> = get(&env, &DataKey::NetworkId)?;
        let mut payload = Bytes::from_array(&env, &network_id.to_array());
        payload.append(&envelope_xdr.slice(0..split));
        let tx_hash = env.crypto().sha256(&payload).to_array();

        check_inclusion(&env, &cp, &tx_hash, index, &proof)?;
        Ok(VerifiedTx {
            ledger_seq,
            ledger_hash: cp.ledger_hash,
            close_time: cp.close_time,
            tx_hash: BytesN::from_array(&env, &tx_hash),
            envelope_type: head.envelope_type,
            source_account: BytesN::from_array(&env, &head.source_account),
            fee_source: head.fee_source.map(|k| BytesN::from_array(&env, &k)),
            fee: head.fee,
            seq_num: head.seq_num,
            op_count: head.op_count,
        })
    }

    // ------------------------------------------------------------ views

    pub fn checkpoint(env: Env, ledger_seq: u32) -> Option<Checkpoint> {
        checkpoint_of(&env, ledger_seq)
    }

    /// Highest checkpointed ledger sequence, if any.
    pub fn latest(env: Env) -> Option<u32> {
        env.storage().instance().get(&DataKey::Latest)
    }

    pub fn network_id(env: Env) -> Result<BytesN<32>, Error> {
        get(&env, &DataKey::NetworkId)
    }

    pub fn relayers(env: Env) -> Result<Vec<Address>, Error> {
        get(&env, &DataKey::Relayers)
    }

    pub fn threshold(env: Env) -> Result<u32, Error> {
        get(&env, &DataKey::Threshold)
    }
}

// ------------------------------------------------------------------ internals

fn get<V: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>(
    env: &Env,
    key: &DataKey,
) -> Result<V, Error> {
    env.storage()
        .instance()
        .get(key)
        .ok_or(Error::NotInitialized)
}

fn checkpoint_of(env: &Env, seq: u32) -> Option<Checkpoint> {
    env.storage().persistent().get(&DataKey::Checkpoint(seq))
}

fn check_inclusion(
    env: &Env,
    cp: &Checkpoint,
    tx_hash: &[u8; 32],
    index: u32,
    proof: &Bytes,
) -> Result<(), Error> {
    let root = merkle::compute_root(env, tx_hash, index, cp.tx_count, proof)
        .map_err(|_| Error::InvalidProof)?;
    if root != cp.tx_root.to_array() {
        return Err(Error::InvalidProof);
    }
    Ok(())
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

/// `1 <= threshold <= n <= MAX_RELAYERS`, no duplicates, strict majority.
fn validate_relayer_set(relayers: &Vec<Address>, threshold: u32) -> Result<(), Error> {
    let n = relayers.len();
    if n == 0 || n > MAX_RELAYERS || threshold == 0 || threshold > n || threshold * 2 <= n {
        return Err(Error::InvalidRelayerSet);
    }
    if has_duplicates(relayers) {
        return Err(Error::InvalidRelayerSet);
    }
    Ok(())
}

/// `signers` must be `threshold` distinct registered relayers, each of which
/// authorizes this exact invocation (arguments included).
fn require_relayer_quorum(env: &Env, signers: &Vec<Address>) -> Result<(), Error> {
    let relayers: Vec<Address> = get(env, &DataKey::Relayers)?;
    let threshold: u32 = get(env, &DataKey::Threshold)?;
    if signers.len() < threshold {
        return Err(Error::InsufficientSignatures);
    }
    if has_duplicates(signers) {
        return Err(Error::DuplicateSigner);
    }
    for s in signers.iter() {
        if !relayers.contains(&s) {
            return Err(Error::NotRelayer);
        }
    }
    for s in signers.iter() {
        s.require_auth();
    }
    Ok(())
}

fn has_duplicates(list: &Vec<Address>) -> bool {
    let n = list.len();
    for i in 0..n {
        for j in (i + 1)..n {
            if list.get_unchecked(i) == list.get_unchecked(j) {
                return true;
            }
        }
    }
    false
}
