extern crate std;

use proptest::prelude::*;
use sha2::{Digest, Sha256};
use soroban_sdk::{
    testutils::{Address as _, AuthorizedFunction},
    vec, Address, Bytes, BytesN, Env, IntoVal, Symbol, Val, Vec,
};
use std::vec::Vec as StdVec;

use crate::merkle;
use crate::xdr_parser::{self, XdrError, ENVELOPE_TYPE_TX, ENVELOPE_TYPE_TX_FEE_BUMP};
use crate::{Error, StateVerifier, StateVerifierClient};

// ======================================================================
// Real Stellar mainnet data (Horizon, ledgers 64670961-64670962, protocol 28)
// ======================================================================

const LEDGER_61: &[u8] = include_bytes!("../testdata/ledger_64670961.xdr");
const LEDGER_62: &[u8] = include_bytes!("../testdata/ledger_64670962.xdr");
const LEDGER_61_HASH: &str = "cc4ee25fa771ddd2152e5af0f60b4275908d1c26b5ba0d7b243978c9616db0ba";
/// All 158 transaction hashes of ledger 64670962 in application order.
const TX_HASHES_62: &[u8] = include_bytes!("../testdata/tx_hashes_64670962.bin");

struct EnvelopeFixture {
    xdr: &'static [u8],
    index: u32,
    signatures_offset: u32,
    tx_hash: &'static str,
    envelope_type: u32,
    source: &'static str,
    fee_source: Option<&'static str>,
    fee: i64,
    seq_num: i64,
    op_count: u32,
}

const V1_SMALL: EnvelopeFixture = EnvelopeFixture {
    xdr: include_bytes!("../testdata/env_v1_small.xdr"),
    index: 105,
    signatures_offset: 128,
    tx_hash: "941757aeb15735083fcef1676395e950eba5feae25036a67b118ba287dab3007",
    envelope_type: ENVELOPE_TYPE_TX,
    source: "dbefd1072127bbf0eab6162ea7c7fc802c2ab9c1372bb3b4207d94ec9a48bd92",
    fee_source: None,
    fee: 10_000,
    seq_num: 246_498_133_146_699_607,
    op_count: 1,
};

const FEE_BUMP: EnvelopeFixture = EnvelopeFixture {
    xdr: include_bytes!("../testdata/env_fee_bump.xdr"),
    index: 1,
    signatures_offset: 416,
    tx_hash: "0a41e004ad5189ce55c25700c3ce622cb9be70138267dcd0d06b966e29937ab0",
    envelope_type: ENVELOPE_TYPE_TX_FEE_BUMP,
    source: "cec40036d4e459d2e4511f460d237c21f816f91173893e240395ca3d97bfc9ec",
    fee_source: Some("280fdd7d577821686333fff2a75809e295cf7e13f23bd9ecf31ff601ae31bff7"),
    fee: 2_000_000,
    seq_num: 214_086_918_361_922_161,
    op_count: 1,
};

const LARGEST: EnvelopeFixture = EnvelopeFixture {
    xdr: include_bytes!("../testdata/env_largest.xdr"),
    index: 131,
    signatures_offset: 14_964,
    tx_hash: "d1efb55d12c519e6f22b894a06d65d70539ae3a568f5699d08211d2c8f035762",
    envelope_type: ENVELOPE_TYPE_TX,
    source: "ba70a1e052b79dbabdba8083aeefcaea3478910241a8da5022ae575c8d599d04",
    fee_source: None,
    fee: 1_000_000,
    seq_num: 259_162_613_021_018_777,
    op_count: 100,
};

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

fn tx_hashes() -> StdVec<[u8; 32]> {
    TX_HASHES_62
        .chunks(32)
        .map(|c| c.try_into().unwrap())
        .collect()
}

fn mainnet_network_id() -> [u8; 32] {
    Sha256::digest(b"Public Global Stellar Network ; September 2015").into()
}

// ======================================================================
// Independent reference Merkle tree (RFC 6962 recursive definition)
// ======================================================================

fn ref_leaf(h: &[u8; 32]) -> [u8; 32] {
    let mut d = Sha256::new();
    d.update([0u8]);
    d.update(h);
    d.finalize().into()
}

fn ref_node(l: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let mut d = Sha256::new();
    d.update([1u8]);
    d.update(l);
    d.update(r);
    d.finalize().into()
}

/// Largest power of two strictly less than `n` (n >= 2).
fn split(n: usize) -> usize {
    let mut k = 1;
    while k * 2 < n {
        k *= 2;
    }
    k
}

fn ref_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.len() == 1 {
        return ref_leaf(&leaves[0]);
    }
    let k = split(leaves.len());
    ref_node(&ref_root(&leaves[..k]), &ref_root(&leaves[k..]))
}

/// RFC 6962 audit path, leaf level first.
fn ref_path(m: usize, leaves: &[[u8; 32]]) -> StdVec<[u8; 32]> {
    if leaves.len() == 1 {
        return StdVec::new();
    }
    let k = split(leaves.len());
    if m < k {
        let mut p = ref_path(m, &leaves[..k]);
        p.push(ref_root(&leaves[k..]));
        p
    } else {
        let mut p = ref_path(m - k, &leaves[k..]);
        p.push(ref_root(&leaves[..k]));
        p
    }
}

fn proof_bytes(env: &Env, path: &[[u8; 32]]) -> Bytes {
    let mut b = Bytes::new(env);
    for s in path {
        b.extend_from_array(s);
    }
    b
}

// ======================================================================
// Contract harness
// ======================================================================

struct Oracle {
    env: Env,
    id: Address,
    admin: Address,
    relayers: [Address; 3],
}

impl Oracle {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.cost_estimate().budget().reset_unlimited();
        let id = env.register(StateVerifier, ());
        Self::init(env, id)
    }

    fn init(env: Env, id: Address) -> Self {
        let admin = Address::generate(&env);
        let relayers = [
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ];
        StateVerifierClient::new(&env, &id).initialize(
            &admin,
            &BytesN::from_array(&env, &mainnet_network_id()),
            &vec![
                &env,
                relayers[0].clone(),
                relayers[1].clone(),
                relayers[2].clone(),
            ],
            &2,
        );
        Oracle {
            env,
            id,
            admin,
            relayers,
        }
    }

    fn c(&self) -> StateVerifierClient<'_> {
        StateVerifierClient::new(&self.env, &self.id)
    }

    fn quorum(&self) -> Vec<Address> {
        vec![
            &self.env,
            self.relayers[0].clone(),
            self.relayers[2].clone(),
        ]
    }

    fn bytes(&self, b: &[u8]) -> Bytes {
        Bytes::from_slice(&self.env, b)
    }

    fn b32(&self, a: &[u8; 32]) -> BytesN<32> {
        BytesN::from_array(&self.env, a)
    }

    /// Checkpoints ledger 64670962 with the root over its real transactions.
    fn checkpoint_62(&self) -> BytesN<32> {
        let hashes = tx_hashes();
        self.c().submit_checkpoint(
            &self.quorum(),
            &self.bytes(LEDGER_62),
            &self.b32(&ref_root(&hashes)),
            &(hashes.len() as u32),
        )
    }

    fn proof_for(&self, index: usize) -> Bytes {
        proof_bytes(&self.env, &ref_path(index, &tx_hashes()))
    }
}

// ======================================================================
// XDR parser on mainnet data
// ======================================================================

#[test]
fn parses_mainnet_ledger_headers() {
    let h61 = xdr_parser::parse_ledger_header(LEDGER_61).unwrap();
    let h62 = xdr_parser::parse_ledger_header(LEDGER_62).unwrap();

    // Values cross-checked against Horizon /ledgers/{seq}.
    assert_eq!((h61.ledger_seq, h62.ledger_seq), (64_670_961, 64_670_962));
    assert_eq!(
        (h62.ledger_version, h62.base_fee, h62.base_reserve),
        (28, 100, 5_000_000)
    );
    assert_eq!(h62.max_tx_set_size, 1_000);
    assert_eq!(h61.close_time, 1_790_640_586); // 2026-09-29T00:09:46Z
    assert_eq!(h62.close_time, 1_790_640_591); // 2026-09-29T00:09:51Z
    assert_eq!(h62.total_coins, 1_054_439_020_873_472_865);

    // Ledger hash is SHA-256 of the header XDR, and the chain links.
    let hash61: [u8; 32] = Sha256::digest(LEDGER_61).into();
    assert_eq!(hash61, hex32(LEDGER_61_HASH));
    assert_eq!(h62.previous_ledger_hash, hash61);
}

#[test]
fn parses_mainnet_envelopes() {
    for f in [&V1_SMALL, &FEE_BUMP, &LARGEST] {
        let head = xdr_parser::parse_envelope_head(f.xdr).unwrap();
        assert_eq!(head.envelope_type, f.envelope_type);
        assert_eq!(head.source_account, hex32(f.source));
        assert_eq!(head.fee_source, f.fee_source.map(hex32));
        assert_eq!(
            (head.fee, head.seq_num, head.op_count),
            (f.fee, f.seq_num, f.op_count)
        );
        assert!(head.end <= f.signatures_offset as usize);
        let sigs = xdr_parser::parse_signatures(&f.xdr[f.signatures_offset as usize..]).unwrap();
        assert!(sigs >= 1);

        // TransactionSignaturePayload hash reproduces Horizon's tx hash.
        let mut d = Sha256::new();
        d.update(mainnet_network_id());
        d.update(&f.xdr[..f.signatures_offset as usize]);
        assert_eq!(<[u8; 32]>::from(d.finalize()), hex32(f.tx_hash));
    }
}

#[test]
fn xdr_decoding_is_strict() {
    use xdr_parser::{parse_envelope_head, parse_ledger_header, parse_signatures};
    assert_eq!(
        parse_ledger_header(&LEDGER_62[..400]),
        Err(XdrError::Truncated)
    );
    let mut extra = LEDGER_62.to_vec();
    extra.extend_from_slice(&[0, 0, 0, 0]);
    assert_eq!(parse_ledger_header(&extra), Err(XdrError::TrailingBytes));
    // StellarValue ext discriminant (offset 4+32+32+8+4 = 80) set to 7.
    let mut bad = LEDGER_62.to_vec();
    bad[80..84].copy_from_slice(&7u32.to_be_bytes());
    assert_eq!(parse_ledger_header(&bad), Err(XdrError::BadDiscriminant));

    // Legacy v0 envelopes and unknown types are rejected.
    let mut v0 = V1_SMALL.xdr.to_vec();
    v0[..4].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(parse_envelope_head(&v0), Err(XdrError::BadDiscriminant));

    // Signature array: count bound, non-zero padding, trailing bytes.
    assert_eq!(
        parse_signatures(&21u32.to_be_bytes()),
        Err(XdrError::TooLong)
    );
    let mut sig = StdVec::new();
    sig.extend_from_slice(&1u32.to_be_bytes());
    sig.extend_from_slice(&[9, 9, 9, 9]); // hint
    sig.extend_from_slice(&3u32.to_be_bytes());
    sig.extend_from_slice(&[1, 2, 3, 0xFF]); // 3 bytes + non-zero pad
    assert_eq!(parse_signatures(&sig), Err(XdrError::BadPadding));
    sig[15] = 0;
    assert_eq!(parse_signatures(&sig), Ok(1));
    sig.push(0);
    assert_eq!(parse_signatures(&sig), Err(XdrError::TrailingBytes));
}

// ======================================================================
// Merkle proofs
// ======================================================================

#[test]
fn merkle_matches_rfc6962_reference_for_all_small_trees() {
    let env = Env::default();
    env.cost_estimate().budget().reset_unlimited();
    for n in 1..=64usize {
        let leaves: StdVec<[u8; 32]> = (0..n)
            .map(|i| Sha256::digest([i as u8, n as u8]).into())
            .collect();
        let root = ref_root(&leaves);
        for (m, leaf) in leaves.iter().enumerate() {
            let path = ref_path(m, &leaves);
            assert_eq!(merkle::path_len(m as u32, n as u32), path.len());
            let got =
                merkle::compute_root(&env, leaf, m as u32, n as u32, &proof_bytes(&env, &path));
            assert_eq!(got, Ok(root), "n={n} m={m}");
        }
    }
}

#[test]
fn merkle_rejects_malformed_proofs() {
    let env = Env::default();
    let leaves: StdVec<[u8; 32]> = (0..5u8).map(|i| [i; 32]).collect();
    let path = ref_path(2, &leaves);
    let good = proof_bytes(&env, &path);
    assert_eq!(
        merkle::compute_root(&env, &leaves[2], 5, 5, &good),
        Err(merkle::ProofError::IndexOutOfRange)
    );
    let short = proof_bytes(&env, &path[..path.len() - 1]);
    assert_eq!(
        merkle::compute_root(&env, &leaves[2], 2, 5, &short),
        Err(merkle::ProofError::BadProofLength)
    );
    // A tree node is not accepted as a leaf (domain separation).
    let node = ref_node(&ref_leaf(&leaves[0]), &ref_leaf(&leaves[1]));
    let root = ref_root(&leaves);
    let as_leaf = merkle::compute_root(
        &env,
        &node,
        0,
        3,
        &proof_bytes(&env, &[ref_leaf(&leaves[2])]),
    );
    assert_ne!(as_leaf, Ok(root));
}

// ======================================================================
// End-to-end on mainnet data
// ======================================================================

#[test]
fn verifies_every_mainnet_transaction_in_the_ledger() {
    let o = Oracle::new();
    let ledger_hash = o.checkpoint_62();
    let expected: [u8; 32] = Sha256::digest(LEDGER_62).into();
    assert_eq!(ledger_hash.to_array(), expected);

    let cp = o.c().checkpoint(&64_670_962).unwrap();
    assert_eq!(
        (cp.tx_count, cp.close_time, cp.ledger_version),
        (158, 1_790_640_591, 28)
    );
    assert_eq!(cp.previous_ledger_hash.to_array(), hex32(LEDGER_61_HASH));
    assert_eq!(o.c().latest(), Some(64_670_962));

    for (i, h) in tx_hashes().iter().enumerate() {
        let got = o
            .c()
            .verify_tx_hash(&64_670_962, &o.b32(h), &(i as u32), &o.proof_for(i));
        assert_eq!(got, ledger_hash);
    }
}

#[test]
fn verifies_mainnet_envelopes_decoded_on_chain() {
    let o = Oracle::new();
    o.checkpoint_62();
    for f in [&V1_SMALL, &FEE_BUMP, &LARGEST] {
        let v = o.c().verify_transaction(
            &64_670_962,
            &o.bytes(f.xdr),
            &f.signatures_offset,
            &f.index,
            &o.proof_for(f.index as usize),
        );
        assert_eq!(
            v.tx_hash.to_array(),
            hex32(f.tx_hash),
            "tx hash computed on-chain"
        );
        assert_eq!(v.envelope_type, f.envelope_type);
        assert_eq!(v.source_account.to_array(), hex32(f.source));
        assert_eq!(v.fee_source.map(|k| k.to_array()), f.fee_source.map(hex32));
        assert_eq!(
            (v.fee, v.seq_num, v.op_count),
            (f.fee, f.seq_num, f.op_count)
        );
        assert_eq!((v.ledger_seq, v.close_time), (64_670_962, 1_790_640_591));
    }
}

#[test]
fn rejects_forged_or_mismatched_inclusion_claims() {
    let o = Oracle::new();
    o.checkpoint_62();
    let f = &V1_SMALL;
    let (xdr, off, idx, proof) = (
        o.bytes(f.xdr),
        f.signatures_offset,
        f.index,
        o.proof_for(f.index as usize),
    );
    let seq = 64_670_962u32;

    // Tampering with any byte of the transaction changes its hash.
    let mut forged = f.xdr.to_vec();
    forged[60] ^= 1;
    assert_eq!(
        o.c()
            .try_verify_transaction(&seq, &o.bytes(&forged), &off, &idx, &proof),
        Err(Ok(Error::InvalidProof))
    );
    // Wrong position, wrong ledger, uncheckpointed ledger.
    assert_eq!(
        o.c()
            .try_verify_transaction(&seq, &xdr, &off, &(idx + 1), &proof),
        Err(Ok(Error::InvalidProof))
    );
    assert_eq!(
        o.c()
            .try_verify_transaction(&(seq + 1), &xdr, &off, &idx, &proof),
        Err(Ok(Error::UnknownLedger))
    );
    // A transaction hash that is not in the ledger.
    assert_eq!(
        o.c()
            .try_verify_tx_hash(&seq, &o.b32(&[7u8; 32]), &idx, &proof),
        Err(Ok(Error::InvalidProof))
    );
    // Offsets that do not split the envelope into tx ‖ signatures<20>.
    for bad in [0u32, 4, off - 4, off + 4, off + 2, xdr.len()] {
        assert_eq!(
            o.c().try_verify_transaction(&seq, &xdr, &bad, &idx, &proof),
            Err(Ok(Error::BadSignaturesOffset)),
            "offset {bad}"
        );
    }
    // A truncated proof.
    let short = proof.slice(0..proof.len() - 32);
    assert_eq!(
        o.c()
            .try_verify_tx_hash(&seq, &o.b32(&hex32(f.tx_hash)), &idx, &short),
        Err(Ok(Error::InvalidProof))
    );
}

/// An envelope whose bytes admit a second, too-early split: bytes 8.. decode
/// as a valid one-signature array, yet the transaction head runs to byte 64.
/// Accepting that split would report fields not covered by the proven hash.
#[test]
fn decoded_fields_must_be_covered_by_the_proven_hash() {
    let mut env_xdr = StdVec::new();
    env_xdr.extend_from_slice(&ENVELOPE_TYPE_TX.to_be_bytes());
    env_xdr.extend_from_slice(&0u32.to_be_bytes()); // KEY_TYPE_ED25519
                                                    // 32-byte key that doubles as: count=1, hint, len=44, first 20 sig bytes
    env_xdr.extend_from_slice(&1u32.to_be_bytes());
    env_xdr.extend_from_slice(&[0xAA; 4]);
    env_xdr.extend_from_slice(&44u32.to_be_bytes());
    env_xdr.extend_from_slice(&[0x55; 20]);
    env_xdr.extend_from_slice(&100u32.to_be_bytes()); // fee
    env_xdr.extend_from_slice(&7u64.to_be_bytes()); // seqNum
    env_xdr.extend_from_slice(&0u32.to_be_bytes()); // PRECOND_NONE
    env_xdr.extend_from_slice(&0u32.to_be_bytes()); // MEMO_NONE
    env_xdr.extend_from_slice(&1u32.to_be_bytes()); // one operation
    assert_eq!(env_xdr.len(), 64);
    assert_eq!(xdr_parser::parse_signatures(&env_xdr[8..]), Ok(1));
    assert_eq!(xdr_parser::parse_envelope_head(&env_xdr).unwrap().end, 64);

    // Relayers commit to the hash of the 4-byte "transaction" that split
    // would produce; the contract must still refuse the split.
    let o = Oracle::new();
    let mut d = Sha256::new();
    d.update(mainnet_network_id());
    d.update(&env_xdr[..8]);
    let short_hash: [u8; 32] = d.finalize().into();
    o.c().submit_checkpoint(
        &o.quorum(),
        &o.bytes(LEDGER_62),
        &o.b32(&ref_leaf(&short_hash)),
        &1,
    );
    assert_eq!(
        o.c()
            .try_verify_transaction(&64_670_962, &o.bytes(&env_xdr), &8, &0, &Bytes::new(&o.env)),
        Err(Ok(Error::BadSignaturesOffset))
    );
}

// ======================================================================
// Checkpointing and hash chain
// ======================================================================

#[test]
fn checkpoints_link_through_previous_ledger_hash() {
    let o = Oracle::new();
    o.checkpoint_62();
    let one = BytesN::from_array(&o.env, &[1u8; 32]);

    // A forged predecessor (one byte changed) does not hash to 62's prev link.
    let mut forged61 = LEDGER_61.to_vec();
    forged61[75] ^= 1; // closeTime low byte
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&o.quorum(), &o.bytes(&forged61), &one, &1),
        Err(Ok(Error::ChainMismatch))
    );
    // The genuine predecessor links.
    o.c()
        .submit_checkpoint(&o.quorum(), &o.bytes(LEDGER_61), &one, &1);
    assert_eq!(o.c().latest(), Some(64_670_962), "latest is monotonic");

    // Re-submission: identical is a no-op, different is a conflict.
    let hashes = tx_hashes();
    let root = o.b32(&ref_root(&hashes));
    o.c()
        .submit_checkpoint(&o.quorum(), &o.bytes(LEDGER_62), &root, &158);
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&o.quorum(), &o.bytes(LEDGER_62), &one, &158),
        Err(Ok(Error::CheckpointConflict))
    );
}

#[test]
fn successor_must_link_to_existing_predecessor() {
    let o = Oracle::new();
    let one = BytesN::from_array(&o.env, &[1u8; 32]);
    o.c()
        .submit_checkpoint(&o.quorum(), &o.bytes(LEDGER_61), &one, &1);
    let mut forged62 = LEDGER_62.to_vec();
    forged62[10] ^= 1; // inside previousLedgerHash
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&o.quorum(), &o.bytes(&forged62), &one, &1),
        Err(Ok(Error::ChainMismatch))
    );
    o.checkpoint_62();
}

#[test]
fn checkpoint_input_validation() {
    let o = Oracle::new();
    let zero = BytesN::from_array(&o.env, &[0u8; 32]);
    let one = BytesN::from_array(&o.env, &[1u8; 32]);
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&o.quorum(), &o.bytes(&LEDGER_62[..300]), &one, &1),
        Err(Ok(Error::MalformedXdr))
    );
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&o.quorum(), &o.bytes(&[0u8; 2000]), &one, &1),
        Err(Ok(Error::MalformedXdr))
    );
    // An empty ledger must commit to the zero root, and vice versa.
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&o.quorum(), &o.bytes(LEDGER_62), &one, &0),
        Err(Ok(Error::InvalidTxRoot))
    );
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&o.quorum(), &o.bytes(LEDGER_62), &zero, &5),
        Err(Ok(Error::InvalidTxRoot))
    );
}

// ======================================================================
// Authorized relayer interface
// ======================================================================

#[test]
fn only_a_relayer_quorum_can_checkpoint() {
    let o = Oracle::new();
    let one = BytesN::from_array(&o.env, &[1u8; 32]);
    let h = o.bytes(LEDGER_62);
    let [r0, r1, _] = o.relayers.clone();
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&vec![&o.env, r0.clone()], &h, &one, &1),
        Err(Ok(Error::InsufficientSignatures))
    );
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&vec![&o.env, r0.clone(), r0.clone()], &h, &one, &1),
        Err(Ok(Error::DuplicateSigner))
    );
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&vec![&o.env, r0.clone(), o.admin.clone()], &h, &one, &1),
        Err(Ok(Error::NotRelayer))
    );
    assert_eq!(o.c().checkpoint(&64_670_962), None);

    // Each relayer's authorization is bound to this exact submission.
    let signers = vec![&o.env, r0.clone(), r1.clone()];
    o.c().submit_checkpoint(&signers, &h, &one, &1);
    let args: Vec<Val> = (signers, h, one, 1u32).into_val(&o.env);
    let auths = o.env.auths();
    for r in [&r0, &r1] {
        let (_, inv) = auths.iter().find(|(a, _)| a == r).expect("relayer signed");
        assert_eq!(
            inv.function,
            AuthorizedFunction::Contract((
                o.id.clone(),
                Symbol::new(&o.env, "submit_checkpoint"),
                args.clone()
            ))
        );
    }
}

#[test]
fn relayer_management() {
    let o = Oracle::new();
    let (a, b, c) = (
        Address::generate(&o.env),
        Address::generate(&o.env),
        Address::generate(&o.env),
    );
    for (set, t) in [
        (vec![&o.env, a.clone(), b.clone()], 1u32),
        (vec![&o.env, a.clone(), a.clone()], 2),
    ] {
        assert_eq!(
            o.c().try_set_relayers(&set, &t),
            Err(Ok(Error::InvalidRelayerSet))
        );
    }
    o.c()
        .set_relayers(&vec![&o.env, a.clone(), b.clone(), c.clone()], &2);
    assert_eq!(o.env.auths()[0].0, o.admin, "admin authorizes rotation");
    assert_eq!(o.c().threshold(), 2);
    // Former relayers are no longer accepted.
    let one = BytesN::from_array(&o.env, &[1u8; 32]);
    assert_eq!(
        o.c()
            .try_submit_checkpoint(&o.quorum(), &o.bytes(LEDGER_62), &one, &1),
        Err(Ok(Error::NotRelayer))
    );
    o.c()
        .submit_checkpoint(&vec![&o.env, b, c], &o.bytes(LEDGER_62), &one, &1);
    assert_eq!(
        o.c().try_initialize(&o.admin, &one, &vec![&o.env, a], &1),
        Err(Ok(Error::AlreadyInitialized))
    );
}

// ======================================================================
// Gas profiling under real WASM metering (network limit: 100M CPU instr.)
// ======================================================================

const TX_CPU_LIMIT: u64 = 100_000_000;

/// Runs the contract compiled to WASM with the default (network) budget and
/// reports CPU / memory for every entry point on mainnet data, plus a
/// worst-case depth-32 proof. Build first with
/// `cargo build --target wasm32v1-none --release`.
#[test]
fn gas_profile_on_mainnet_data() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/target/wasm32v1-none/release/state_verifier.wasm"
    );
    let Ok(wasm) = std::fs::read(path) else {
        std::eprintln!("skipping gas profile: build the WASM first");
        return;
    };
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();
    let id = env.register(wasm.as_slice(), ());
    let o = Oracle::init(env, id);

    let measure = |label: &str, f: &dyn Fn()| {
        o.env.cost_estimate().budget().reset_default();
        f();
        let b = o.env.cost_estimate().budget();
        let (cpu, mem) = (b.cpu_instruction_cost(), b.memory_bytes_cost());
        std::eprintln!(
            "{label:<42} cpu {cpu:>10}  mem {mem:>9}  ({:.2}% of limit)",
            cpu as f64 / TX_CPU_LIMIT as f64 * 100.0
        );
        assert!(cpu < TX_CPU_LIMIT, "{label} exceeds CPU limit");
        cpu
    };

    measure("submit_checkpoint (428-byte header)", &|| {
        o.checkpoint_62();
    });
    measure("verify_tx_hash (158 txs, depth 8)", &|| {
        o.c().verify_tx_hash(
            &64_670_962,
            &o.b32(&hex32(V1_SMALL.tx_hash)),
            &V1_SMALL.index,
            &o.proof_for(105),
        );
    });
    for (label, f) in [
        ("verify_transaction v1 (204 B)", &V1_SMALL),
        ("verify_transaction fee-bump (492 B)", &FEE_BUMP),
        ("verify_transaction 100 ops (15 KB)", &LARGEST),
    ] {
        measure(label, &|| {
            o.c().verify_transaction(
                &64_670_962,
                &o.bytes(f.xdr),
                &f.signatures_offset,
                &f.index,
                &o.proof_for(f.index as usize),
            );
        });
    }

    // Worst case: depth-32 proof (tree of 2^32 - 1 leaves), built by folding
    // arbitrary siblings with the reference hash functions.
    o.env.cost_estimate().budget().reset_unlimited();
    let leaf_count = u32::MAX;
    let index = 0u32;
    let depth = merkle::path_len(index, leaf_count);
    assert_eq!(depth, 32);
    let siblings: StdVec<[u8; 32]> = (0..depth).map(|i| [i as u8 + 1; 32]).collect();
    let tx = hex32(V1_SMALL.tx_hash);
    let root = siblings.iter().fold(ref_leaf(&tx), |h, s| ref_node(&h, s));
    let mut hdr = LEDGER_62.to_vec();
    hdr[252..256].copy_from_slice(&1u32.to_be_bytes()); // distinct ledgerSeq: 1
    o.c()
        .submit_checkpoint(&o.quorum(), &o.bytes(&hdr), &o.b32(&root), &leaf_count);
    let seq = xdr_parser::parse_ledger_header(&hdr).unwrap().ledger_seq;
    let cpu = measure("verify_tx_hash worst case (depth 32)", &|| {
        o.c()
            .verify_tx_hash(&seq, &o.b32(&tx), &index, &proof_bytes(&o.env, &siblings));
    });
    assert!(
        cpu < TX_CPU_LIMIT / 10,
        "worst-case proof stays under 10% of the limit"
    );
}

// ======================================================================
// Fuzzing: the parsers never panic and reject garbage safely
// ======================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn parsers_never_panic_on_arbitrary_input(bytes in prop::collection::vec(any::<u8>(), 0..1600)) {
        let _ = xdr_parser::parse_ledger_header(&bytes);
        let _ = xdr_parser::parse_envelope_head(&bytes);
        let _ = xdr_parser::parse_signatures(&bytes);
    }

    #[test]
    fn mutated_mainnet_header_either_parses_or_errors(pos in 0usize..428, byte in any::<u8>()) {
        let mut h = LEDGER_62.to_vec();
        h[pos] = byte;
        if xdr_parser::parse_ledger_header(&h).is_ok() {
            // Any accepted mutation is reflected faithfully in its hash.
            let changed = h != LEDGER_62;
            let hash: [u8; 32] = Sha256::digest(&h).into();
            prop_assert_eq!(changed, hash != <[u8; 32]>::from(Sha256::digest(LEDGER_62)));
        }
    }
}
