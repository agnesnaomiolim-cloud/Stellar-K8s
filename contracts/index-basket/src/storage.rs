use soroban_sdk::{contracttype, panic_with_error, Address, Env, String, Vec};

use crate::{Component, Error, RebalanceConfig};

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const PERSISTENT_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const PERSISTENT_THRESHOLD: u32 = PERSISTENT_BUMP - 7 * DAY_IN_LEDGERS;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowanceKey {
    pub from: Address,
    pub spender: Address,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowanceValue {
    pub amount: i128,
    pub expiration_ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    pub name: String,
    pub symbol: String,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Metadata,
    Config,
    Components,
    Reserves,
    Supply,
    Balance(Address),
    Arbitrageur(Address),
    Allowance(AllowanceKey),
}

pub fn extend_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

fn instance_get<V: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>(env: &Env, key: &DataKey) -> V {
    env.storage()
        .instance()
        .get(key)
        .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
}

pub fn admin(env: &Env) -> Address {
    instance_get(env, &DataKey::Admin)
}

pub fn set_admin(env: &Env, admin: &Address) {
    env.storage().instance().set(&DataKey::Admin, admin);
}

pub fn metadata(env: &Env) -> Metadata {
    instance_get(env, &DataKey::Metadata)
}

pub fn set_metadata(env: &Env, metadata: &Metadata) {
    env.storage().instance().set(&DataKey::Metadata, metadata);
}

pub fn config(env: &Env) -> RebalanceConfig {
    instance_get(env, &DataKey::Config)
}

pub fn set_config(env: &Env, config: &RebalanceConfig) {
    env.storage().instance().set(&DataKey::Config, config);
}

pub fn components(env: &Env) -> Vec<Component> {
    instance_get(env, &DataKey::Components)
}

pub fn set_components(env: &Env, components: &Vec<Component>) {
    env.storage()
        .instance()
        .set(&DataKey::Components, components);
}

/// Internally tracked holdings, index-aligned with `components`. Tokens sent
/// to the contract outside `issue`/`rebalance` are ignored, so donations
/// cannot skew issuance ratios.
pub fn reserves(env: &Env) -> Vec<i128> {
    instance_get(env, &DataKey::Reserves)
}

pub fn set_reserves(env: &Env, reserves: &Vec<i128>) {
    env.storage().instance().set(&DataKey::Reserves, reserves);
}

pub fn supply(env: &Env) -> i128 {
    env.storage().instance().get(&DataKey::Supply).unwrap_or(0)
}

pub fn set_supply(env: &Env, supply: i128) {
    env.storage().instance().set(&DataKey::Supply, &supply);
}

pub fn balance(env: &Env, id: &Address) -> i128 {
    let key = DataKey::Balance(id.clone());
    match env.storage().persistent().get::<_, i128>(&key) {
        Some(b) => {
            env.storage()
                .persistent()
                .extend_ttl(&key, PERSISTENT_THRESHOLD, PERSISTENT_BUMP);
            b
        }
        None => 0,
    }
}

pub fn set_balance(env: &Env, id: &Address, amount: i128) {
    let key = DataKey::Balance(id.clone());
    env.storage().persistent().set(&key, &amount);
    env.storage()
        .persistent()
        .extend_ttl(&key, PERSISTENT_THRESHOLD, PERSISTENT_BUMP);
}

pub fn is_arbitrageur(env: &Env, id: &Address) -> bool {
    env.storage()
        .persistent()
        .has(&DataKey::Arbitrageur(id.clone()))
}

pub fn set_arbitrageur(env: &Env, id: &Address, allowed: bool) {
    let key = DataKey::Arbitrageur(id.clone());
    if allowed {
        env.storage().persistent().set(&key, &());
        env.storage()
            .persistent()
            .extend_ttl(&key, PERSISTENT_THRESHOLD, PERSISTENT_BUMP);
    } else {
        env.storage().persistent().remove(&key);
    }
}

pub fn allowance(env: &Env, from: &Address, spender: &Address) -> AllowanceValue {
    let key = DataKey::Allowance(AllowanceKey {
        from: from.clone(),
        spender: spender.clone(),
    });
    match env.storage().temporary().get::<_, AllowanceValue>(&key) {
        Some(a) if a.expiration_ledger >= env.ledger().sequence() => a,
        _ => AllowanceValue {
            amount: 0,
            expiration_ledger: 0,
        },
    }
}

pub fn set_allowance(
    env: &Env,
    from: &Address,
    spender: &Address,
    amount: i128,
    expiration_ledger: u32,
) {
    if amount > 0 && expiration_ledger < env.ledger().sequence() {
        panic_with_error!(env, Error::InvalidExpiration);
    }
    let key = DataKey::Allowance(AllowanceKey {
        from: from.clone(),
        spender: spender.clone(),
    });
    env.storage().temporary().set(
        &key,
        &AllowanceValue {
            amount,
            expiration_ledger,
        },
    );
    if amount > 0 {
        let live_for = expiration_ledger - env.ledger().sequence();
        env.storage()
            .temporary()
            .extend_ttl(&key, live_for, live_for);
    }
}
