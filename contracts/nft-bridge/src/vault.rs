//! Bridge vault: escrow of native NFTs, relayer-gated minting of synthetic
//! NFTs, and the replay / double-mint guards that keep supply parity.
//!
//! Supply parity (see SECURITY.md §2):
//! * every native NFT is either circulating on Soroban **or** escrowed here
//!   with a live wrapped twin on Ethereum — never both;
//! * every Ethereum NFT locked in the Ethereum vault has **at most one** live
//!   synthetic here, and a synthetic exists only while its origin is locked.

use soroban_sdk::{contracttype, symbol_short, Address, BytesN, Env, String, Vec};

use crate::storage::{self, DataKey};
use crate::{Error, Token, TokenKind};

/// Hard cap on the relayer federation size (bounds quorum-check cost).
pub const MAX_RELAYERS: u32 = 20;

/// Identity of an ERC-721 token on Ethereum.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EthOrigin {
    /// EIP-155 chain id the token lives on.
    pub chain_id: u64,
    /// ERC-721 contract address.
    pub contract: BytesN<20>,
    /// ERC-721 `uint256` token id, big-endian.
    pub token_id: BytesN<32>,
}

/// Relayer attestation that an ERC-721 was locked in the Ethereum vault.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MintAttestation {
    /// Unique id of the Ethereum lock event (e.g. keccak(tx hash ‖ log index)).
    pub lock_id: BytesN<32>,
    pub origin: EthOrigin,
    pub recipient: Address,
    pub token_uri: String,
}

/// Relayer attestation that the wrapped twin of an escrowed native NFT was
/// burned on Ethereum, releasing the original.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnlockAttestation {
    /// Unique id of the Ethereum burn event.
    pub burn_id: BytesN<32>,
    pub token_id: u128,
    pub recipient: Address,
}

/// Escrow record of a native NFT that is currently bridged to Ethereum.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockRecord {
    pub nonce: u64,
    pub dest_chain_id: u64,
    pub dest_address: BytesN<20>,
    pub locked_by: Address,
}

/// Supply accounting used to prove parity.
#[contracttype]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BridgeStats {
    /// Native NFTs ever minted (never decreases; natives are never burned).
    pub native_minted: u64,
    /// Native NFTs currently escrowed (== wrapped twins live on Ethereum).
    pub native_locked: u64,
    /// Synthetic NFTs currently live (== Ethereum NFTs locked for Soroban).
    pub synthetic_live: u64,
}

// ------------------------------------------------------------ relayer quorum

/// Validates a relayer set: `1 <= threshold <= len <= MAX_RELAYERS`, no
/// duplicates, and a strict-majority threshold so two disjoint subsets of
/// the federation can never both reach quorum.
pub fn validate_relayer_set(relayers: &Vec<Address>, threshold: u32) -> Result<(), Error> {
    let n = relayers.len();
    if n == 0 || n > MAX_RELAYERS || threshold == 0 || threshold > n || threshold * 2 <= n {
        return Err(Error::InvalidRelayerSet);
    }
    if has_duplicates(relayers) {
        return Err(Error::InvalidRelayerSet);
    }
    Ok(())
}

/// Enforces that `signers` is a quorum of the registered relayers and that
/// every one of them authorized this exact invocation (arguments included,
/// so each authorization is bound to the attestation being executed).
pub fn require_relayer_quorum(env: &Env, signers: &Vec<Address>) -> Result<(), Error> {
    let relayers = storage::relayers(env)?;
    let threshold = storage::threshold(env)?;
    if signers.len() < threshold {
        return Err(Error::InsufficientSignatures);
    }
    if has_duplicates(signers) {
        return Err(Error::DuplicateSigner);
    }
    for signer in signers.iter() {
        if !relayers.contains(&signer) {
            return Err(Error::NotRelayer);
        }
    }
    for signer in signers.iter() {
        signer.require_auth();
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

// ------------------------------------------------------------ bridge flows

/// Escrows a **native** NFT and emits the outbound bridge event.
pub fn lock(
    env: &Env,
    caller: &Address,
    token_id: u128,
    dest_chain_id: u64,
    dest_address: BytesN<20>,
) -> Result<u64, Error> {
    if dest_chain_id != storage::eth_chain_id(env)? {
        return Err(Error::UnsupportedChain);
    }
    if dest_address == BytesN::from_array(env, &[0u8; 20]) {
        return Err(Error::InvalidDestination);
    }
    let mut token = storage::token(env, token_id)?;
    // Synthetic NFTs must never enter the native vault pool.
    if token.kind != TokenKind::Native {
        return Err(Error::CannotLockSynthetic);
    }
    let vault = env.current_contract_address();
    if token.owner == vault {
        return Err(Error::TokenLocked);
    }
    storage::require_can_manage(env, caller, &token)?;

    let nonce = storage::next_nonce(env);
    let from = token.owner.clone();
    storage::move_token(env, token_id, &mut token, &from, &vault);
    storage::set_lock(
        env,
        token_id,
        &LockRecord {
            nonce,
            dest_chain_id,
            dest_address: dest_address.clone(),
            locked_by: from.clone(),
        },
    );
    let mut stats = storage::stats(env);
    stats.native_locked += 1;
    storage::set_stats(env, &stats);

    env.events().publish(
        (symbol_short!("bridge"), symbol_short!("lock"), from),
        (nonce, token_id, dest_chain_id, dest_address, token.uri),
    );
    Ok(nonce)
}

/// Releases an escrowed native NFT after its wrapped twin was burned on
/// Ethereum. Relayer quorum only.
pub fn unlock(env: &Env, signers: &Vec<Address>, att: &UnlockAttestation) -> Result<(), Error> {
    require_relayer_quorum(env, signers)?;
    let consumed = DataKey::ConsumedBurn(att.burn_id.clone());
    if storage::has(env, &consumed) {
        return Err(Error::AttestationReplayed);
    }
    let vault = env.current_contract_address();
    if att.recipient == vault {
        return Err(Error::InvalidRecipient);
    }
    let mut token = storage::token(env, att.token_id)?;
    if token.kind != TokenKind::Native || storage::lock_of(env, att.token_id).is_none() {
        return Err(Error::NotLocked);
    }

    storage::mark(env, &consumed);
    storage::remove_lock(env, att.token_id);
    storage::move_token(env, att.token_id, &mut token, &vault, &att.recipient);
    let mut stats = storage::stats(env);
    stats.native_locked -= 1;
    storage::set_stats(env, &stats);

    env.events().publish(
        (
            symbol_short!("bridge"),
            symbol_short!("unlock"),
            att.recipient.clone(),
        ),
        (att.burn_id.clone(), att.token_id),
    );
    Ok(())
}

/// Mints the synthetic representation of an ERC-721 locked on Ethereum.
/// Relayer quorum only; each Ethereum lock and each origin NFT can back at
/// most one live synthetic.
pub fn mint_synthetic(
    env: &Env,
    signers: &Vec<Address>,
    att: &MintAttestation,
) -> Result<u128, Error> {
    require_relayer_quorum(env, signers)?;
    if att.origin.chain_id != storage::eth_chain_id(env)? {
        return Err(Error::UnsupportedChain);
    }
    let consumed = DataKey::ConsumedLock(att.lock_id.clone());
    if storage::has(env, &consumed) {
        return Err(Error::AttestationReplayed);
    }
    let live = DataKey::SyntheticOf(att.origin.clone());
    if storage::has(env, &live) {
        return Err(Error::AlreadyMinted);
    }
    if att.recipient == env.current_contract_address() {
        return Err(Error::InvalidRecipient);
    }

    let token_id = storage::next_token_id(env);
    let token = Token {
        owner: att.recipient.clone(),
        kind: TokenKind::Synthetic(att.origin.clone()),
        uri: att.token_uri.clone(),
        approved: None,
    };
    storage::create_token(env, token_id, &token);
    storage::mark(env, &consumed);
    storage::set_synthetic_of(env, &att.origin, token_id);
    let mut stats = storage::stats(env);
    stats.synthetic_live += 1;
    storage::set_stats(env, &stats);

    env.events().publish(
        (
            symbol_short!("bridge"),
            symbol_short!("minted"),
            att.recipient.clone(),
        ),
        (att.lock_id.clone(), token_id, att.origin.clone()),
    );
    Ok(token_id)
}

/// Burns a synthetic NFT so relayers release the original on Ethereum.
pub fn burn_synthetic(
    env: &Env,
    caller: &Address,
    token_id: u128,
    dest_address: BytesN<20>,
) -> Result<u64, Error> {
    if dest_address == BytesN::from_array(env, &[0u8; 20]) {
        return Err(Error::InvalidDestination);
    }
    let token = storage::token(env, token_id)?;
    let TokenKind::Synthetic(origin) = token.kind.clone() else {
        return Err(Error::NotSynthetic);
    };
    storage::require_can_manage(env, caller, &token)?;

    let nonce = storage::next_nonce(env);
    storage::destroy_token(env, token_id, &token);
    storage::remove(env, &DataKey::SyntheticOf(origin.clone()));
    let mut stats = storage::stats(env);
    stats.synthetic_live -= 1;
    storage::set_stats(env, &stats);

    env.events().publish(
        (symbol_short!("bridge"), symbol_short!("burn"), token.owner),
        (nonce, token_id, origin, dest_address),
    );
    Ok(nonce)
}
