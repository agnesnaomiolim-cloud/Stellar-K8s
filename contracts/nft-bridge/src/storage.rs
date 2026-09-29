//! Typed storage access. Every ownership change goes through `move_token`,
//! `create_token` or `destroy_token`, which keep balances and approvals
//! consistent with the token record.

use soroban_sdk::{contracttype, Address, BytesN, Env, IntoVal, String, Val, Vec};

use crate::vault::{BridgeStats, EthOrigin, LockRecord};
use crate::{Error, Token};

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const PERSISTENT_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const PERSISTENT_THRESHOLD: u32 = PERSISTENT_BUMP - 7 * DAY_IN_LEDGERS;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    // instance
    Admin,
    Name,
    Symbol,
    EthChainId,
    Relayers,
    Threshold,
    NextTokenId,
    Nonce,
    Stats,
    // persistent
    Token(u128),
    Balance(Address),
    Operator(Address, Address),
    Lock(u128),
    SyntheticOf(EthOrigin),
    ConsumedLock(BytesN<32>),
    ConsumedBurn(BytesN<32>),
}

// ------------------------------------------------------------------ config

pub fn is_initialized(env: &Env) -> bool {
    env.storage().instance().has(&DataKey::Admin)
}

#[allow(clippy::too_many_arguments)]
pub fn init(
    env: &Env,
    admin: &Address,
    name: &String,
    symbol: &String,
    eth_chain_id: u64,
    relayers: &Vec<Address>,
    threshold: u32,
) {
    let s = env.storage().instance();
    s.set(&DataKey::Admin, admin);
    s.set(&DataKey::Name, name);
    s.set(&DataKey::Symbol, symbol);
    s.set(&DataKey::EthChainId, &eth_chain_id);
    s.set(&DataKey::Relayers, relayers);
    s.set(&DataKey::Threshold, &threshold);
    s.set(&DataKey::NextTokenId, &1u128);
    s.set(&DataKey::Nonce, &0u64);
    s.set(&DataKey::Stats, &BridgeStats::default());
    bump_instance(env);
}

fn instance_get<V: soroban_sdk::TryFromVal<Env, Val>>(
    env: &Env,
    key: &DataKey,
) -> Result<V, Error> {
    env.storage()
        .instance()
        .get(key)
        .ok_or(Error::NotInitialized)
}

pub fn admin(env: &Env) -> Result<Address, Error> {
    instance_get(env, &DataKey::Admin)
}

pub fn name(env: &Env) -> Result<String, Error> {
    instance_get(env, &DataKey::Name)
}

pub fn symbol(env: &Env) -> Result<String, Error> {
    instance_get(env, &DataKey::Symbol)
}

pub fn eth_chain_id(env: &Env) -> Result<u64, Error> {
    instance_get(env, &DataKey::EthChainId)
}

pub fn relayers(env: &Env) -> Result<Vec<Address>, Error> {
    instance_get(env, &DataKey::Relayers)
}

pub fn threshold(env: &Env) -> Result<u32, Error> {
    instance_get(env, &DataKey::Threshold)
}

pub fn set_relayers(env: &Env, relayers: &Vec<Address>, threshold: u32) {
    env.storage().instance().set(&DataKey::Relayers, relayers);
    env.storage()
        .instance()
        .set(&DataKey::Threshold, &threshold);
    bump_instance(env);
}

pub fn next_token_id(env: &Env) -> u128 {
    let id: u128 = env
        .storage()
        .instance()
        .get(&DataKey::NextTokenId)
        .unwrap_or(1);
    env.storage()
        .instance()
        .set(&DataKey::NextTokenId, &(id + 1));
    id
}

/// Monotonic nonce shared by all outbound (Soroban → Ethereum) events.
pub fn next_nonce(env: &Env) -> u64 {
    let n: u64 = env.storage().instance().get(&DataKey::Nonce).unwrap_or(0);
    env.storage().instance().set(&DataKey::Nonce, &(n + 1));
    n
}

pub fn stats(env: &Env) -> BridgeStats {
    env.storage()
        .instance()
        .get(&DataKey::Stats)
        .unwrap_or_default()
}

pub fn set_stats(env: &Env, stats: &BridgeStats) {
    env.storage().instance().set(&DataKey::Stats, stats);
    bump_instance(env);
}

pub fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

// -------------------------------------------------------------- persistent

fn save<V: IntoVal<Env, Val>>(env: &Env, key: &DataKey, value: &V) {
    let s = env.storage().persistent();
    s.set(key, value);
    s.extend_ttl(key, PERSISTENT_THRESHOLD, PERSISTENT_BUMP);
}

pub fn has(env: &Env, key: &DataKey) -> bool {
    env.storage().persistent().has(key)
}

pub fn mark(env: &Env, key: &DataKey) {
    save(env, key, &true);
}

pub fn remove(env: &Env, key: &DataKey) {
    env.storage().persistent().remove(key);
}

pub fn token(env: &Env, token_id: u128) -> Result<Token, Error> {
    env.storage()
        .persistent()
        .get(&DataKey::Token(token_id))
        .ok_or(Error::TokenNotFound)
}

pub fn balance(env: &Env, owner: &Address) -> u64 {
    env.storage()
        .persistent()
        .get(&DataKey::Balance(owner.clone()))
        .unwrap_or(0)
}

fn add_balance(env: &Env, owner: &Address, delta: i64) {
    let b = balance(env, owner)
        .checked_add_signed(delta)
        .expect("balance invariant");
    let key = DataKey::Balance(owner.clone());
    if b == 0 {
        remove(env, &key);
    } else {
        save(env, &key, &b);
    }
}

pub fn is_operator(env: &Env, owner: &Address, operator: &Address) -> bool {
    env.storage()
        .persistent()
        .get(&DataKey::Operator(owner.clone(), operator.clone()))
        .unwrap_or(false)
}

pub fn set_operator(env: &Env, owner: &Address, operator: &Address, approved: bool) {
    let key = DataKey::Operator(owner.clone(), operator.clone());
    if approved {
        save(env, &key, &true);
    } else {
        remove(env, &key);
    }
}

pub fn save_token(env: &Env, token_id: u128, token: &Token) {
    save(env, &DataKey::Token(token_id), token);
}

pub fn create_token(env: &Env, token_id: u128, token: &Token) {
    save_token(env, token_id, token);
    add_balance(env, &token.owner, 1);
}

pub fn destroy_token(env: &Env, token_id: u128, token: &Token) {
    remove(env, &DataKey::Token(token_id));
    add_balance(env, &token.owner, -1);
}

/// Transfers ownership, clearing any single-token approval (ERC-721 rule).
pub fn move_token(env: &Env, token_id: u128, token: &mut Token, from: &Address, to: &Address) {
    token.owner = to.clone();
    token.approved = None;
    save_token(env, token_id, token);
    add_balance(env, from, -1);
    add_balance(env, to, 1);
}

/// `caller` must authorize and be the owner, the approved address, or an
/// operator of the owner. Escrowed tokens (owned by the vault) are frozen.
pub fn require_can_manage(env: &Env, caller: &Address, token: &Token) -> Result<(), Error> {
    if token.owner == env.current_contract_address() {
        return Err(Error::TokenLocked);
    }
    caller.require_auth();
    let allowed = *caller == token.owner
        || token.approved.as_ref() == Some(caller)
        || is_operator(env, &token.owner, caller);
    if allowed {
        Ok(())
    } else {
        Err(Error::NotAuthorized)
    }
}

pub fn lock_of(env: &Env, token_id: u128) -> Option<LockRecord> {
    env.storage().persistent().get(&DataKey::Lock(token_id))
}

pub fn set_lock(env: &Env, token_id: u128, record: &LockRecord) {
    save(env, &DataKey::Lock(token_id), record);
}

pub fn remove_lock(env: &Env, token_id: u128) {
    remove(env, &DataKey::Lock(token_id));
}

pub fn synthetic_of(env: &Env, origin: &EthOrigin) -> Option<u128> {
    env.storage()
        .persistent()
        .get(&DataKey::SyntheticOf(origin.clone()))
}

pub fn set_synthetic_of(env: &Env, origin: &EthOrigin, token_id: u128) {
    save(env, &DataKey::SyntheticOf(origin.clone()), &token_id);
}
