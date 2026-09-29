//! # XDR Parser
//!
//! A brutally-optimised, zero-allocation XDR decoder for the Stellar types that
//! matter for light-client state verification:
//!
//! * [`LedgerHeader`]  – the header anchored in every ledger bucket-list hash.
//! * [`TransactionEnvelope`] – a stripped-down transaction wrapper whose hash
//!   can be proven inside a ledger's transaction-set Merkle tree.
//!
//! ## Design Constraints
//!
//! * **`no_std` / `no_heap`** – all parsing is slice-based; no allocations are
//!   required for the happy path.  `Vec` is only used where the SDK requires it.
//! * **Instruction budget** – every function is O(1) or O(n) in input bytes.
//!   No nested loops, no recursion, no dynamic dispatch.
//! * **XDR subset** – only the fixed-layout fields needed for Merkle root
//!   reconstruction are decoded.  Variable-length arrays are length-validated
//!   and then skipped via a counted cursor advance.

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

use sha2::{Digest, Sha256};

#[cfg(test)]
extern crate alloc;

/// A 32-byte hash (SHA-256 / BLAKE-based digest as returned by Stellar Core).
pub type Hash32 = [u8; 32];

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors emitted by the XDR parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdrError {
    /// The byte slice ended before the expected field could be read.
    UnexpectedEof,
    /// A variable-length sequence exceeded the declared maximum.
    OverflowLen,
    /// An enum discriminant value was not in the known set.
    UnknownVariant,
    /// A flag or boolean field contained an illegal value.
    BadFlag,
}

// ---------------------------------------------------------------------------
// Cursor — a thin no-alloc slice reader
// ---------------------------------------------------------------------------

/// A byte-cursor over a borrowed XDR buffer.
///
/// All reads are big-endian, consistent with XDR specification (RFC 4506).
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// Wrap a byte slice.
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Remaining unread bytes.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Read exactly `n` bytes, advancing the cursor.
    #[inline]
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], XdrError> {
        let end = self.pos.checked_add(n).ok_or(XdrError::OverflowLen)?;
        if end > self.buf.len() {
            return Err(XdrError::UnexpectedEof);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Skip exactly `n` bytes without copying.
    #[inline]
    pub fn skip(&mut self, n: usize) -> Result<(), XdrError> {
        let end = self.pos.checked_add(n).ok_or(XdrError::OverflowLen)?;
        if end > self.buf.len() {
            return Err(XdrError::UnexpectedEof);
        }
        self.pos = end;
        Ok(())
    }

    /// Read a big-endian `u32`.
    #[inline]
    pub fn read_u32(&mut self) -> Result<u32, XdrError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    /// Read a big-endian `i32`.
    #[inline]
    pub fn read_i32(&mut self) -> Result<i32, XdrError> {
        let bytes = self.read_bytes(4)?;
        Ok(i32::from_be_bytes(bytes.try_into().unwrap()))
    }

    /// Read a big-endian `u64`.
    #[inline]
    pub fn read_u64(&mut self) -> Result<u64, XdrError> {
        let bytes = self.read_bytes(8)?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    /// Read a big-endian `i64`.
    #[inline]
    pub fn read_i64(&mut self) -> Result<i64, XdrError> {
        let bytes = self.read_bytes(8)?;
        Ok(i64::from_be_bytes(bytes.try_into().unwrap()))
    }

    /// Read an XDR `opaque<32>` (exactly 32 bytes) into a `Hash32`.
    #[inline]
    pub fn read_hash32(&mut self) -> Result<Hash32, XdrError> {
        let bytes = self.read_bytes(32)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    /// Read an XDR variable-length opaque (length-prefixed).
    ///
    /// Returns the raw byte slice from the original buffer.  Padding bytes
    /// (XDR aligns to 4-byte boundaries) are consumed but not returned.
    ///
    /// `max_len` bounds the declared length to guard against DoS.
    #[inline]
    pub fn read_var_opaque(&mut self, max_len: u32) -> Result<&'a [u8], XdrError> {
        let len = self.read_u32()?;
        if len > max_len {
            return Err(XdrError::OverflowLen);
        }
        let data = self.read_bytes(len as usize)?;
        // XDR pads to 4-byte boundary
        let pad = (4 - (len % 4)) % 4;
        self.skip(pad as usize)?;
        Ok(data)
    }

    /// Skip an XDR variable-length string (same wire format as var_opaque).
    #[inline]
    pub fn skip_string(&mut self, max_len: u32) -> Result<(), XdrError> {
        self.read_var_opaque(max_len).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// LedgerHeader
// ---------------------------------------------------------------------------

/// The fields of a Stellar `LedgerHeader` relevant for state verification.
///
/// Full wire layout (subset decoded here):
/// ```text
/// uint32       ledgerVersion
/// Hash         previousLedgerHash    (32 bytes)
/// StellarValue scpValue              (variable – skipped)
/// Hash         txSetResultHash       (32 bytes)
/// Hash         bucketListHash        (32 bytes)
/// uint32       ledgerSeq
/// int64        totalCoins
/// int64        feePool
/// uint32       inflationSeq
/// uint32       idPool
/// uint32       baseFee
/// uint32       baseReserve
/// uint32       maxTxSetSize
/// Hash[4]      skipList              (4 × 32 bytes)
/// ext                                (skipped)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerHeader {
    /// Protocol version active at this ledger.
    pub ledger_version: u32,
    /// Hash of the immediately preceding ledger.
    pub previous_ledger_hash: Hash32,
    /// SHA-256 hash of the full transaction-result set for this ledger.
    pub tx_set_result_hash: Hash32,
    /// SHA-256 root of the bucket-list (the world state snapshot).
    pub bucket_list_hash: Hash32,
    /// Ledger sequence number (1-based).
    pub ledger_seq: u32,
    /// Total XLM in existence (in stroops).
    pub total_coins: i64,
    /// Accumulated fee pool (in stroops).
    pub fee_pool: i64,
    /// Inflation sequence counter.
    pub inflation_seq: u32,
    /// Auto-incrementing ID pool.
    pub id_pool: u32,
    /// Base fee per operation (in stroops).
    pub base_fee: u32,
    /// Minimum account balance (in stroops).
    pub base_reserve: u32,
    /// Maximum number of operations per transaction set.
    pub max_tx_set_size: u32,
    /// Skip-list hashes (4 × previous checkpoint ledgers).
    pub skip_list: [Hash32; 4],
}

impl LedgerHeader {
    /// Decode a `LedgerHeader` from raw XDR bytes.
    ///
    /// # Errors
    /// Returns [`XdrError`] if the slice is too short or contains invalid data.
    pub fn from_xdr(bytes: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(bytes);

        let ledger_version = c.read_u32()?;
        let previous_ledger_hash = c.read_hash32()?;

        // StellarValue: skip it — it contains the close time, upgrades, etc.
        // Wire: uint32 closeTime (8 bytes as u64), then upgrades array, ext.
        // We skip conservatively using a max-size opaque guard.
        // StellarValue = Upgrades (array) + closeTime + ext.
        // Minimum 12 bytes; we allow up to 4 KiB.
        skip_stellar_value(&mut c)?;

        let tx_set_result_hash = c.read_hash32()?;
        let bucket_list_hash = c.read_hash32()?;
        let ledger_seq = c.read_u32()?;
        let total_coins = c.read_i64()?;
        let fee_pool = c.read_i64()?;
        let inflation_seq = c.read_u32()?;
        let id_pool = c.read_u32()?;
        let base_fee = c.read_u32()?;
        let base_reserve = c.read_u32()?;
        let max_tx_set_size = c.read_u32()?;

        let skip_list = [
            c.read_hash32()?,
            c.read_hash32()?,
            c.read_hash32()?,
            c.read_hash32()?,
        ];

        // Skip the ext union (version 0 = 4-byte zero; later versions vary).
        // We tolerate any remaining bytes.

        Ok(LedgerHeader {
            ledger_version,
            previous_ledger_hash,
            tx_set_result_hash,
            bucket_list_hash,
            ledger_seq,
            total_coins,
            fee_pool,
            inflation_seq,
            id_pool,
            base_fee,
            base_reserve,
            max_tx_set_size,
            skip_list,
        })
    }

    /// Compute the SHA-256 hash of the raw XDR bytes (the "ledger hash").
    ///
    /// This matches the value Stellar Core stores in `previousLedgerHash` of
    /// the next ledger.
    #[inline]
    pub fn hash_xdr(xdr_bytes: &[u8]) -> Hash32 {
        let digest = Sha256::digest(xdr_bytes);
        digest.into()
    }
}

// ---------------------------------------------------------------------------
// StellarValue skip helper
// ---------------------------------------------------------------------------

/// Skip a `StellarValue` union in the cursor.
///
/// `StellarValue` wire format:
/// ```text
/// Hash         txSetHash          (32 bytes)
/// TimePoint    closeTime          (8 bytes / u64)
/// upgrades<6>  LedgerUpgrade[]    (var array, each item var_opaque<128>)
/// StellarValueExt ext             (union tag u32; v0 = nothing extra)
/// ```
fn skip_stellar_value(c: &mut Cursor<'_>) -> Result<(), XdrError> {
    // txSetHash (32 bytes)
    c.skip(32)?;
    // closeTime (u64)
    c.skip(8)?;
    // upgrades array: length-prefixed, up to 6 entries of var_opaque<128>
    let upgrade_count = c.read_u32()?;
    if upgrade_count > 6 {
        return Err(XdrError::OverflowLen);
    }
    for _ in 0..upgrade_count {
        c.read_var_opaque(128)?;
    }
    // ext: discriminant u32 (0 = empty; 1 = LedgerCloseValueSignature)
    let ext_type = c.read_u32()?;
    match ext_type {
        0 => {}
        1 => {
            // LedgerCloseValueSignature: nodeID (32 bytes) + signature var_opaque<64>
            c.skip(32)?;
            c.read_var_opaque(64)?;
        }
        _ => return Err(XdrError::UnknownVariant),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// TransactionEnvelope (stripped)
// ---------------------------------------------------------------------------

/// A minimal decoded transaction envelope.
///
/// Only the fields needed to reconstruct the transaction hash are captured;
/// everything else is skipped to stay within instruction limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionEnvelope {
    /// The account that originated the transaction.
    pub source_account: [u8; 32],
    /// Fee offered (in stroops).
    pub fee: u32,
    /// Sequence number of the source account.
    pub seq_num: i64,
    /// Ledger at which this transaction becomes valid (inclusive).
    pub min_ledger_seq: u32,
    /// Ledger at which this transaction expires (inclusive).
    pub max_ledger_seq: u32,
    /// SHA-256 of the full raw XDR envelope (the "transaction hash").
    pub envelope_hash: Hash32,
}

impl TransactionEnvelope {
    /// Decode a `TransactionEnvelope` from raw XDR bytes.
    ///
    /// Supports both v0 and v1 envelope types.
    pub fn from_xdr(bytes: &[u8]) -> Result<Self, XdrError> {
        let mut c = Cursor::new(bytes);

        // Envelope type discriminant
        let env_type = c.read_i32()?;
        match env_type {
            // ENVELOPE_TYPE_TX (v1 = 2)
            2 => Self::decode_v1(&mut c, bytes),
            // ENVELOPE_TYPE_TX_V0 (0) — legacy pre-protocol-13 format
            0 => Self::decode_v0(&mut c, bytes),
            _ => Err(XdrError::UnknownVariant),
        }
    }

    /// Decode a v1 transaction envelope (protocol ≥ 13).
    fn decode_v1(c: &mut Cursor<'_>, raw: &[u8]) -> Result<Self, XdrError> {
        // TransactionV1 inner union
        // source: MuxedAccount — type(4) + key(32) = 36 or type(4) + id(8) + key(32) = 44
        let source_type = c.read_u32()?;
        let source_account = match source_type {
            0 => c.read_hash32()?, // KEY_TYPE_ED25519
            256 => {
                // KEY_TYPE_MUXED_ED25519
                c.skip(8)?; // muxed ID
                c.read_hash32()?
            }
            _ => return Err(XdrError::UnknownVariant),
        };

        let fee = c.read_u32()?;
        let seq_num = c.read_i64()?;

        // TimeBounds: optional discriminant
        let has_time_bounds = c.read_u32()?;
        if has_time_bounds == 1 {
            c.skip(16)?; // minTime + maxTime (2 × u64)
        } else if has_time_bounds != 0 {
            return Err(XdrError::BadFlag);
        }

        // LedgerBounds: optional (protocol ≥ 19)
        let has_ledger_bounds = c.read_u32()?;
        let (min_ls, max_ls) = if has_ledger_bounds == 1 {
            let mn = c.read_u32()?;
            let mx = c.read_u32()?;
            (mn, mx)
        } else if has_ledger_bounds == 0 {
            (0, 0)
        } else {
            return Err(XdrError::BadFlag);
        };

        // Remaining fields (minSeqNum, minSeqAge, minSeqLedgerGap, extra signers)
        // are skipped — we only need source/fee/seq for identification.
        // Skip operations array: length-prefixed variable ops
        // But for hashing we use the raw bytes, so we just stop here.

        let envelope_hash = compute_transaction_hash(raw);

        Ok(TransactionEnvelope {
            source_account,
            fee,
            seq_num,
            min_ledger_seq: min_ls,
            max_ledger_seq: max_ls,
            envelope_hash,
        })
    }

    /// Decode a v0 (legacy) transaction envelope.
    fn decode_v0(c: &mut Cursor<'_>, raw: &[u8]) -> Result<Self, XdrError> {
        // v0: source is plain Ed25519 key (32 bytes)
        let source_account = c.read_hash32()?;
        let fee = c.read_u32()?;
        let seq_num = c.read_i64()?;

        // Optional timeBounds
        let has_tb = c.read_u32()?;
        if has_tb == 1 {
            c.skip(16)?;
        }

        // No ledger bounds in v0 — skip the memo field instead
        // (we don't decode memo; remaining fields just advance cursor)

        let envelope_hash = compute_transaction_hash(raw);

        Ok(TransactionEnvelope {
            source_account,
            fee,
            seq_num,
            min_ledger_seq: 0,
            max_ledger_seq: 0,
            envelope_hash,
        })
    }
}

// ---------------------------------------------------------------------------
// Hash helpers
// ---------------------------------------------------------------------------

/// Compute the canonical Stellar transaction hash.
///
/// Stellar hashes transaction envelopes as:
/// `SHA-256(network_id || ENVELOPE_TYPE_TX || inner_tx_bytes)`
///
/// For simplicity in the on-chain verifier (where the network passphrase is
/// pre-agreed), we compute `SHA-256(xdr_bytes)` directly.  Callers that need
/// the full network-tagged hash should pre-hash at the off-chain proof
/// generation step and supply the resulting 32-byte digest.
#[inline]
pub fn compute_transaction_hash(xdr_bytes: &[u8]) -> Hash32 {
    let digest = Sha256::digest(xdr_bytes);
    digest.into()
}

/// Hash a state payload for use as a Merkle leaf.
///
/// Uses a domain-separation prefix to distinguish state leaves from internal
/// Merkle nodes, preventing second-preimage attacks.
#[inline]
pub fn hash_state_payload(payload: &[u8]) -> Hash32 {
    let mut hasher = Sha256::new();
    hasher.update(b"\x00"); // leaf prefix (domain separator)
    hasher.update(payload);
    hasher.finalize().into()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Build a minimal synthetic XDR buffer for a LedgerHeader and verify
    /// that all fields round-trip correctly.
    #[test]
    fn test_ledger_header_roundtrip() {
        // Construct a minimal XDR-like byte stream:
        // ledgerVersion(4) + previousLedgerHash(32)
        // + StellarValue(32+8+4+4=48 minimal, 0 upgrades, ext=0)
        // + txSetResultHash(32) + bucketListHash(32)
        // + ledgerSeq(4) + totalCoins(8) + feePool(8)
        // + inflationSeq(4) + idPool(4) + baseFee(4) + baseReserve(4) + maxTxSetSize(4)
        // + skipList (4 × 32 = 128)

        let mut buf = Vec::new();

        // ledgerVersion = 20
        buf.extend_from_slice(&20u32.to_be_bytes());

        // previousLedgerHash = all 0xAA
        buf.extend_from_slice(&[0xAAu8; 32]);

        // StellarValue:
        //   txSetHash (32 bytes of 0xBB)
        buf.extend_from_slice(&[0xBBu8; 32]);
        //   closeTime (u64 = 1_700_000_000)
        buf.extend_from_slice(&1_700_000_000u64.to_be_bytes());
        //   upgrades count = 0
        buf.extend_from_slice(&0u32.to_be_bytes());
        //   ext discriminant = 0
        buf.extend_from_slice(&0u32.to_be_bytes());

        // txSetResultHash = all 0xCC
        buf.extend_from_slice(&[0xCCu8; 32]);

        // bucketListHash = all 0xDD
        buf.extend_from_slice(&[0xDDu8; 32]);

        // ledgerSeq = 1234
        buf.extend_from_slice(&1234u32.to_be_bytes());

        // totalCoins
        buf.extend_from_slice(&(500_000_000_000i64).to_be_bytes());

        // feePool
        buf.extend_from_slice(&(1_000_000i64).to_be_bytes());

        // inflationSeq
        buf.extend_from_slice(&0u32.to_be_bytes());

        // idPool
        buf.extend_from_slice(&42u32.to_be_bytes());

        // baseFee
        buf.extend_from_slice(&100u32.to_be_bytes());

        // baseReserve
        buf.extend_from_slice(&5_000_000u32.to_be_bytes());

        // maxTxSetSize
        buf.extend_from_slice(&1000u32.to_be_bytes());

        // skipList (4 × 32 bytes)
        for _ in 0..4 {
            buf.extend_from_slice(&[0x11u8; 32]);
        }

        let hdr = LedgerHeader::from_xdr(&buf).expect("parse should succeed");

        assert_eq!(hdr.ledger_version, 20);
        assert_eq!(hdr.previous_ledger_hash, [0xAAu8; 32]);
        assert_eq!(hdr.tx_set_result_hash, [0xCCu8; 32]);
        assert_eq!(hdr.bucket_list_hash, [0xDDu8; 32]);
        assert_eq!(hdr.ledger_seq, 1234);
        assert_eq!(hdr.base_fee, 100);
        assert_eq!(hdr.skip_list, [[0x11u8; 32]; 4]);
    }

    #[test]
    fn test_hash_state_payload_deterministic() {
        let h1 = hash_state_payload(b"test payload");
        let h2 = hash_state_payload(b"test payload");
        assert_eq!(h1, h2);

        // Different inputs must produce different hashes
        let h3 = hash_state_payload(b"different payload");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_cursor_eof() {
        let buf = [0u8; 2];
        let mut c = Cursor::new(&buf);
        assert!(c.read_u32().is_err());
    }

    #[test]
    fn test_var_opaque_max_len() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&200u32.to_be_bytes()); // length = 200
        buf.extend(core::iter::repeat(0u8).take(200));
        buf.extend_from_slice(&[0u8; 0]); // no padding needed (200 % 4 == 0)
        let mut c = Cursor::new(&buf);
        // max_len = 100 should reject length 200
        assert_eq!(c.read_var_opaque(100), Err(XdrError::OverflowLen));
    }
}
