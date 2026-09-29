//! Minimal, allocation-free XDR decoding for Stellar ledger headers and
//! transaction envelopes, following `Stellar-ledger.x` / `Stellar-transaction.x`.
//!
//! The reader works on a borrowed byte slice with no copying and bounds-checks
//! every read. Decoding is strict: unknown union arms, oversize variable-length
//! fields and non-zero XDR padding are rejected.

/// XDR decoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XdrError {
    /// Input ended before the structure was complete.
    Truncated,
    /// A union discriminant or enum value is not supported.
    BadDiscriminant,
    /// A variable-length field exceeds its declared maximum.
    TooLong,
    /// XDR padding bytes were not zero.
    BadPadding,
    /// Bytes remained after a structure that must fill the whole input.
    TrailingBytes,
}

pub type Hash = [u8; 32];

/// Sequential XDR reader over a byte slice.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], XdrError> {
        let end = self.pos.checked_add(n).ok_or(XdrError::Truncated)?;
        let out = self.buf.get(self.pos..end).ok_or(XdrError::Truncated)?;
        self.pos = end;
        Ok(out)
    }

    pub fn u32(&mut self) -> Result<u32, XdrError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, XdrError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    pub fn i64(&mut self) -> Result<i64, XdrError> {
        Ok(self.u64()? as i64)
    }

    pub fn hash(&mut self) -> Result<Hash, XdrError> {
        let mut h = [0u8; 32];
        h.copy_from_slice(self.take(32)?);
        Ok(h)
    }

    /// Skips `n` bytes of fixed-size data (must already be a multiple of 4).
    pub fn skip(&mut self, n: usize) -> Result<(), XdrError> {
        self.take(n).map(|_| ())
    }

    /// Variable-length `opaque<max>` / `string<max>`, including zero padding.
    pub fn var_opaque(&mut self, max: u32) -> Result<&'a [u8], XdrError> {
        let len = self.u32()?;
        if len > max {
            return Err(XdrError::TooLong);
        }
        let data = self.take(len as usize)?;
        let pad = (4 - (len as usize % 4)) % 4;
        if self.take(pad)?.iter().any(|&b| b != 0) {
            return Err(XdrError::BadPadding);
        }
        Ok(data)
    }

    /// XDR optional (`T*`): returns whether the value is present.
    pub fn present(&mut self) -> Result<bool, XdrError> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(XdrError::BadDiscriminant),
        }
    }

    /// Union / enum discriminant or array length bounded by `max`.
    pub fn bounded(&mut self, max: u32) -> Result<u32, XdrError> {
        let v = self.u32()?;
        if v > max {
            return Err(XdrError::TooLong);
        }
        Ok(v)
    }

    pub fn finish(&self) -> Result<(), XdrError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(XdrError::TrailingBytes)
        }
    }
}

// ================================================================ LedgerHeader

/// Decoded `LedgerHeader` (fields needed for verification and checkpointing).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerHeader {
    pub ledger_version: u32,
    pub previous_ledger_hash: Hash,
    /// `scpValue.txSetHash`: SHA-256 of the ledger's (generalized) tx set.
    pub tx_set_hash: Hash,
    /// `scpValue.closeTime` (UNIX seconds).
    pub close_time: u64,
    pub tx_set_result_hash: Hash,
    pub bucket_list_hash: Hash,
    pub ledger_seq: u32,
    pub total_coins: i64,
    pub fee_pool: i64,
    pub base_fee: u32,
    pub base_reserve: u32,
    pub max_tx_set_size: u32,
}

/// Decodes a complete XDR `LedgerHeader`. The whole input must be consumed.
pub fn parse_ledger_header(buf: &[u8]) -> Result<LedgerHeader, XdrError> {
    let mut r = Reader::new(buf);
    let ledger_version = r.u32()?;
    let previous_ledger_hash = r.hash()?;

    // StellarValue scpValue
    let tx_set_hash = r.hash()?;
    let close_time = r.u64()?;
    let upgrades = r.bounded(6)?; // UpgradeType upgrades<6>
    for _ in 0..upgrades {
        r.var_opaque(128)?; // UpgradeType = opaque<128>
    }
    match r.u32()? {
        0 => {} // STELLAR_VALUE_BASIC
        1 => {
            // STELLAR_VALUE_SIGNED: LedgerCloseValueSignature
            if r.u32()? != 0 {
                return Err(XdrError::BadDiscriminant); // PUBLIC_KEY_TYPE_ED25519
            }
            r.skip(32)?; // nodeID
            r.var_opaque(64)?; // Signature
        }
        _ => return Err(XdrError::BadDiscriminant),
    }

    let tx_set_result_hash = r.hash()?;
    let bucket_list_hash = r.hash()?;
    let ledger_seq = r.u32()?;
    let total_coins = r.i64()?;
    let fee_pool = r.i64()?;
    r.skip(4 + 8)?; // inflationSeq, idPool
    let base_fee = r.u32()?;
    let base_reserve = r.u32()?;
    let max_tx_set_size = r.u32()?;
    r.skip(4 * 32)?; // skipList[4]

    match r.u32()? {
        0 => {}
        1 => {
            // LedgerHeaderExtensionV1 { uint32 flags; ext { case 0: void } }
            r.skip(4)?;
            if r.u32()? != 0 {
                return Err(XdrError::BadDiscriminant);
            }
        }
        _ => return Err(XdrError::BadDiscriminant),
    }
    r.finish()?;

    Ok(LedgerHeader {
        ledger_version,
        previous_ledger_hash,
        tx_set_hash,
        close_time,
        tx_set_result_hash,
        bucket_list_hash,
        ledger_seq,
        total_coins,
        fee_pool,
        base_fee,
        base_reserve,
        max_tx_set_size,
    })
}

// ============================================================ TransactionEnvelope

pub const ENVELOPE_TYPE_TX: u32 = 2;
pub const ENVELOPE_TYPE_TX_FEE_BUMP: u32 = 5;

const KEY_TYPE_ED25519: u32 = 0;
const KEY_TYPE_MUXED_ED25519: u32 = 0x100;

/// Header fields of a transaction envelope. Only the leading fields are
/// decoded; operations and the transaction extension are covered by the
/// transaction hash rather than decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TxHead {
    /// `ENVELOPE_TYPE_TX` or `ENVELOPE_TYPE_TX_FEE_BUMP`.
    pub envelope_type: u32,
    /// Ed25519 key of the (inner) transaction source account.
    pub source_account: Hash,
    /// Ed25519 key of the fee-bump fee source (fee-bump envelopes only).
    pub fee_source: Option<Hash>,
    /// Maximum fee: the inner `uint32 fee`, or the outer `int64 fee` of a fee bump.
    pub fee: i64,
    pub seq_num: i64,
    pub op_count: u32,
    /// Byte offset just past the last decoded field.
    pub end: usize,
}

/// `MuxedAccount` → underlying Ed25519 key.
fn muxed_account(r: &mut Reader) -> Result<Hash, XdrError> {
    match r.u32()? {
        KEY_TYPE_ED25519 => r.hash(),
        KEY_TYPE_MUXED_ED25519 => {
            r.skip(8)?; // uint64 id
            r.hash()
        }
        _ => Err(XdrError::BadDiscriminant),
    }
}

/// `SignerKey` (used by `PreconditionsV2.extraSigners`).
fn skip_signer_key(r: &mut Reader) -> Result<(), XdrError> {
    match r.u32()? {
        0..=2 => r.skip(32), // ED25519, PRE_AUTH_TX, HASH_X
        3 => {
            // ED25519_SIGNED_PAYLOAD { uint256 ed25519; opaque payload<64>; }
            r.skip(32)?;
            r.var_opaque(64).map(|_| ())
        }
        _ => Err(XdrError::BadDiscriminant),
    }
}

/// `Preconditions` union.
fn skip_preconditions(r: &mut Reader) -> Result<(), XdrError> {
    match r.u32()? {
        0 => Ok(()),     // PRECOND_NONE
        1 => r.skip(16), // PRECOND_TIME: TimeBounds
        2 => {
            // PRECOND_V2: PreconditionsV2
            if r.present()? {
                r.skip(16)?; // TimeBounds*
            }
            if r.present()? {
                r.skip(8)?; // LedgerBounds*
            }
            if r.present()? {
                r.skip(8)?; // SequenceNumber* minSeqNum
            }
            r.skip(8 + 4)?; // minSeqAge, minSeqLedgerGap
            let n = r.bounded(2)?; // SignerKey extraSigners<2>
            for _ in 0..n {
                skip_signer_key(r)?;
            }
            Ok(())
        }
        _ => Err(XdrError::BadDiscriminant),
    }
}

/// `Memo` union.
fn skip_memo(r: &mut Reader) -> Result<(), XdrError> {
    match r.u32()? {
        0 => Ok(()),                       // MEMO_NONE
        1 => r.var_opaque(28).map(|_| ()), // MEMO_TEXT string<28>
        2 => r.skip(8),                    // MEMO_ID
        3 | 4 => r.skip(32),               // MEMO_HASH, MEMO_RETURN
        _ => Err(XdrError::BadDiscriminant),
    }
}

/// `Transaction` fields up to and including the operation count.
fn tx_v1_head(r: &mut Reader) -> Result<(Hash, u32, i64, u32), XdrError> {
    let source = muxed_account(r)?;
    let fee = r.u32()?;
    let seq = r.i64()?;
    skip_preconditions(r)?;
    skip_memo(r)?;
    let ops = r.u32()?;
    if ops == 0 || ops > 100 {
        return Err(XdrError::TooLong); // Operation operations<MAX_OPS_PER_TX>
    }
    Ok((source, fee, seq, ops))
}

/// Decodes the head of a `TransactionEnvelope` (v1 or fee-bump). `buf`
/// starts at the envelope discriminant and may be a prefix of the envelope.
/// Legacy `ENVELOPE_TYPE_TX_V0` envelopes are rejected.
pub fn parse_envelope_head(buf: &[u8]) -> Result<TxHead, XdrError> {
    let mut r = Reader::new(buf);
    let envelope_type = r.u32()?;
    let head = match envelope_type {
        ENVELOPE_TYPE_TX => {
            let (source_account, fee, seq_num, op_count) = tx_v1_head(&mut r)?;
            TxHead {
                envelope_type,
                source_account,
                fee_source: None,
                fee: fee as i64,
                seq_num,
                op_count,
                end: 0,
            }
        }
        ENVELOPE_TYPE_TX_FEE_BUMP => {
            // FeeBumpTransaction { MuxedAccount feeSource; int64 fee; innerTx; ext }
            let fee_source = muxed_account(&mut r)?;
            let fee = r.i64()?;
            if r.u32()? != ENVELOPE_TYPE_TX {
                return Err(XdrError::BadDiscriminant); // innerTx must be a v1 envelope
            }
            let (source_account, _inner_fee, seq_num, op_count) = tx_v1_head(&mut r)?;
            TxHead {
                envelope_type,
                source_account,
                fee_source: Some(fee_source),
                fee,
                seq_num,
                op_count,
                end: 0,
            }
        }
        _ => return Err(XdrError::BadDiscriminant),
    };
    Ok(TxHead {
        end: r.position(),
        ..head
    })
}

/// Maximum size of a `DecoratedSignature signatures<20>` array in bytes.
pub const MAX_SIGNATURES_LEN: usize = 4 + 20 * (4 + 4 + 64);

/// Validates that `buf` is exactly one `DecoratedSignature signatures<20>`
/// array (the envelope tail). Returns the signature count.
pub fn parse_signatures(buf: &[u8]) -> Result<u32, XdrError> {
    let mut r = Reader::new(buf);
    let n = r.bounded(20)?;
    for _ in 0..n {
        r.skip(4)?; // SignatureHint
        r.var_opaque(64)?; // Signature
    }
    r.finish()?;
    Ok(n)
}
