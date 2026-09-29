//! Cross-chain NFT bridge vault (ERC-721 ⇄ Soroban).
//!
//! One contract is both an ERC-721-style collection and the bridge vault:
//!
//! * **Native** NFTs are minted here by the collection admin. Bridging one to
//!   Ethereum escrows it in the vault ([`NftBridge::lock`]); relayers release
//!   it again once its wrapped twin is burned on Ethereum ([`NftBridge::unlock`]).
//! * **Synthetic** NFTs represent ERC-721s locked in the Ethereum vault. Only a
//!   quorum of the federated relayers can mint them
//!   ([`NftBridge::mint_synthetic`]); holders burn them to bridge back
//!   ([`NftBridge::burn_synthetic`]).
//!
//! The ERC-721 surface maps `msg.sender` checks onto Soroban `require_auth`:
//! every state-changing call names the acting address and requires its
//! authorization. See `SECURITY.md` for the privilege audit.

#![no_std]

mod storage;
pub mod vault;

#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, BytesN, Env,
    String, Vec,
};

pub use vault::{BridgeStats, EthOrigin, LockRecord, MintAttestation, UnlockAttestation};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAuthorized = 3,
    TokenNotFound = 4,
    InvalidRecipient = 5,
    TokenLocked = 6,
    CannotLockSynthetic = 7,
    NotSynthetic = 8,
    NotLocked = 9,
    AlreadyMinted = 10,
    AttestationReplayed = 11,
    UnsupportedChain = 12,
    InvalidDestination = 13,
    InvalidRelayerSet = 14,
    InsufficientSignatures = 15,
    DuplicateSigner = 16,
    NotRelayer = 17,
}

/// Where a token comes from.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    /// Minted on Soroban; may be escrowed in the vault to bridge out.
    Native,
    /// Synthetic representation of an ERC-721 locked on Ethereum.
    Synthetic(EthOrigin),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub owner: Address,
    pub kind: TokenKind,
    pub uri: String,
    pub approved: Option<Address>,
}

#[contract]
pub struct NftBridge;

#[contractimpl]
impl NftBridge {
    /// One-time setup. `relayers`/`threshold` form the federated multi-sig
    /// (strict majority required); `eth_chain_id` is the EIP-155 id of the
    /// counterpart Ethereum network.
    pub fn initialize(
        env: Env,
        admin: Address,
        name: String,
        symbol: String,
        eth_chain_id: u64,
        relayers: Vec<Address>,
        threshold: u32,
    ) -> Result<(), Error> {
        if storage::is_initialized(&env) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        vault::validate_relayer_set(&relayers, threshold)?;
        storage::init(
            &env,
            &admin,
            &name,
            &symbol,
            eth_chain_id,
            &relayers,
            threshold,
        );
        Ok(())
    }

    // ------------------------------------------------------ ERC-721 surface

    pub fn name(env: Env) -> Result<String, Error> {
        storage::name(&env)
    }

    pub fn symbol(env: Env) -> Result<String, Error> {
        storage::symbol(&env)
    }

    /// Tokens currently in existence (native, including escrowed, plus live
    /// synthetics).
    pub fn total_supply(env: Env) -> u64 {
        let s = storage::stats(&env);
        s.native_minted + s.synthetic_live
    }

    pub fn balance_of(env: Env, owner: Address) -> u64 {
        storage::balance(&env, &owner)
    }

    pub fn owner_of(env: Env, token_id: u128) -> Result<Address, Error> {
        Ok(storage::token(&env, token_id)?.owner)
    }

    pub fn token_uri(env: Env, token_id: u128) -> Result<String, Error> {
        Ok(storage::token(&env, token_id)?.uri)
    }

    pub fn get_approved(env: Env, token_id: u128) -> Result<Option<Address>, Error> {
        Ok(storage::token(&env, token_id)?.approved)
    }

    pub fn is_approved_for_all(env: Env, owner: Address, operator: Address) -> bool {
        storage::is_operator(&env, &owner, &operator)
    }

    /// Sets (or clears, with `None`) the single-token approval. `caller` must
    /// be the owner or one of the owner's operators.
    pub fn approve(
        env: Env,
        caller: Address,
        approved: Option<Address>,
        token_id: u128,
    ) -> Result<(), Error> {
        caller.require_auth();
        let mut token = storage::token(&env, token_id)?;
        if token.owner == env.current_contract_address() {
            return Err(Error::TokenLocked);
        }
        if caller != token.owner && !storage::is_operator(&env, &token.owner, &caller) {
            return Err(Error::NotAuthorized);
        }
        token.approved = approved.clone();
        storage::save_token(&env, token_id, &token);
        env.events().publish(
            (symbol_short!("approve"), token.owner),
            (approved, token_id),
        );
        Ok(())
    }

    pub fn set_approval_for_all(
        env: Env,
        owner: Address,
        operator: Address,
        approved: bool,
    ) -> Result<(), Error> {
        owner.require_auth();
        if owner == operator {
            return Err(Error::NotAuthorized);
        }
        storage::set_operator(&env, &owner, &operator, approved);
        env.events()
            .publish((symbol_short!("appr_all"), owner), (operator, approved));
        Ok(())
    }

    /// ERC-721 `transferFrom`. `spender` must authorize and be the owner, the
    /// approved address or an operator. Transfers into the vault address are
    /// rejected: the only way into escrow is [`NftBridge::lock`].
    pub fn transfer_from(
        env: Env,
        spender: Address,
        from: Address,
        to: Address,
        token_id: u128,
    ) -> Result<(), Error> {
        let mut token = storage::token(&env, token_id)?;
        storage::require_can_manage(&env, &spender, &token)?;
        if token.owner != from {
            return Err(Error::NotAuthorized);
        }
        if to == env.current_contract_address() {
            return Err(Error::InvalidRecipient);
        }
        storage::move_token(&env, token_id, &mut token, &from, &to);
        env.events()
            .publish((symbol_short!("transfer"), from, to), token_id);
        Ok(())
    }

    // ------------------------------------------------------ native issuance

    /// Mints a new native NFT. Collection admin only; the admin has no
    /// power over synthetic minting or the vault.
    pub fn mint_native(env: Env, to: Address, token_uri: String) -> Result<u128, Error> {
        storage::admin(&env)?.require_auth();
        if to == env.current_contract_address() {
            return Err(Error::InvalidRecipient);
        }
        let token_id = storage::next_token_id(&env);
        storage::create_token(
            &env,
            token_id,
            &Token {
                owner: to.clone(),
                kind: TokenKind::Native,
                uri: token_uri,
                approved: None,
            },
        );
        let mut stats = storage::stats(&env);
        stats.native_minted += 1;
        storage::set_stats(&env, &stats);
        env.events().publish((symbol_short!("mint"), to), token_id);
        Ok(token_id)
    }

    // ------------------------------------------------------ bridge

    /// Escrows a native NFT for bridging to `dest_address` on Ethereum and
    /// emits `("bridge", "lock", owner)` with
    /// `(nonce, token_id, dest_chain_id, dest_address, token_uri)`.
    pub fn lock(
        env: Env,
        caller: Address,
        token_id: u128,
        dest_chain_id: u64,
        dest_address: BytesN<20>,
    ) -> Result<u64, Error> {
        vault::lock(&env, &caller, token_id, dest_chain_id, dest_address)
    }

    /// Releases an escrowed native NFT. Relayer quorum only.
    pub fn unlock(
        env: Env,
        signers: Vec<Address>,
        attestation: UnlockAttestation,
    ) -> Result<(), Error> {
        vault::unlock(&env, &signers, &attestation)
    }

    /// Mints the synthetic twin of an ERC-721 locked on Ethereum. Relayer
    /// quorum only.
    pub fn mint_synthetic(
        env: Env,
        signers: Vec<Address>,
        attestation: MintAttestation,
    ) -> Result<u128, Error> {
        vault::mint_synthetic(&env, &signers, &attestation)
    }

    /// Burns a synthetic NFT and emits `("bridge", "burn", owner)` with
    /// `(nonce, token_id, origin, dest_address)` so relayers release the
    /// original on Ethereum.
    pub fn burn_synthetic(
        env: Env,
        caller: Address,
        token_id: u128,
        dest_address: BytesN<20>,
    ) -> Result<u64, Error> {
        vault::burn_synthetic(&env, &caller, token_id, dest_address)
    }

    /// Rotates the relayer federation. Requires a quorum of the *current*
    /// relayers; the admin cannot change the set.
    pub fn set_relayers(
        env: Env,
        signers: Vec<Address>,
        relayers: Vec<Address>,
        threshold: u32,
    ) -> Result<(), Error> {
        vault::require_relayer_quorum(&env, &signers)?;
        vault::validate_relayer_set(&relayers, threshold)?;
        storage::set_relayers(&env, &relayers, threshold);
        env.events().publish(
            (symbol_short!("bridge"), symbol_short!("relayers")),
            (relayers, threshold),
        );
        Ok(())
    }

    // ------------------------------------------------------ bridge views

    pub fn token_kind(env: Env, token_id: u128) -> Result<TokenKind, Error> {
        Ok(storage::token(&env, token_id)?.kind)
    }

    pub fn lock_of(env: Env, token_id: u128) -> Option<LockRecord> {
        storage::lock_of(&env, token_id)
    }

    /// Live synthetic token id for an Ethereum NFT, if any.
    pub fn synthetic_of(env: Env, origin: EthOrigin) -> Option<u128> {
        storage::synthetic_of(&env, &origin)
    }

    pub fn is_lock_consumed(env: Env, lock_id: BytesN<32>) -> bool {
        storage::has(&env, &storage::DataKey::ConsumedLock(lock_id))
    }

    pub fn is_burn_consumed(env: Env, burn_id: BytesN<32>) -> bool {
        storage::has(&env, &storage::DataKey::ConsumedBurn(burn_id))
    }

    pub fn relayers(env: Env) -> Result<Vec<Address>, Error> {
        storage::relayers(&env)
    }

    pub fn threshold(env: Env) -> Result<u32, Error> {
        storage::threshold(&env)
    }

    pub fn stats(env: Env) -> BridgeStats {
        storage::stats(&env)
    }
}
