//! Index basket factory.
//!
//! Deploys `index-basket` contracts from an uploaded WASM hash and keeps an
//! on-chain registry of every basket it created. Deployment addresses are
//! derived from `sha256(creator || salt)`, so a creator's address cannot be
//! squatted by front-running the same salt.

#![no_std]

#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short,
    xdr::ToXdr, Address, BytesN, Env, String, Vec,
};

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const PERSISTENT_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const PERSISTENT_THRESHOLD: u32 = PERSISTENT_BUMP - 7 * DAY_IN_LEDGERS;
/// Page size cap for `baskets`.
pub const MAX_PAGE: u32 = 50;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    /// The basket admin must be the creator authorising the deployment.
    AdminMismatch = 2,
}

/// Mirror of `index_basket::ComponentInit` (identical XDR encoding).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentInit {
    pub token: Address,
    pub weight_bps: u32,
}

/// Mirror of `index_basket::BasketParams` (identical XDR encoding). Validation
/// happens in the basket constructor, which reverts the whole deployment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasketParams {
    pub admin: Address,
    pub name: String,
    pub symbol: String,
    pub components: Vec<ComponentInit>,
    pub oracle: Address,
    pub initial_nav: i128,
    pub max_price_age: u64,
    pub tolerance_bps: u32,
    pub incentive_bps: u32,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    WasmHash,
    Count,
    Basket(u32),
    IsBasket(Address),
}

#[contract]
pub struct BasketFactory;

#[contractimpl]
impl BasketFactory {
    pub fn __constructor(env: Env, admin: Address, basket_wasm_hash: BytesN<32>) {
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::WasmHash, &basket_wasm_hash);
        env.storage().instance().set(&DataKey::Count, &0u32);
        bump_instance(&env);
    }

    /// Deploys and registers a new basket. `params.admin` must equal
    /// `creator`. Reverts atomically if the basket constructor rejects the
    /// parameters.
    pub fn create_basket(
        env: Env,
        creator: Address,
        salt: BytesN<32>,
        params: BasketParams,
    ) -> Address {
        creator.require_auth();
        if params.admin != creator {
            panic_with_error!(&env, Error::AdminMismatch);
        }
        bump_instance(&env);

        let wasm_hash: BytesN<32> = get_instance(&env, &DataKey::WasmHash);
        let basket = env
            .deployer()
            .with_current_contract(scoped_salt(&env, &creator, &salt))
            .deploy_v2(wasm_hash, (params,));

        let id: u32 = get_instance(&env, &DataKey::Count);
        set_persistent(&env, &DataKey::Basket(id), &basket);
        set_persistent(&env, &DataKey::IsBasket(basket.clone()), &id);
        env.storage().instance().set(&DataKey::Count, &(id + 1));

        env.events()
            .publish((symbol_short!("created"), creator), (id, basket.clone()));
        basket
    }

    /// Address `create_basket(creator, salt, ..)` deploys to.
    pub fn basket_address(env: Env, creator: Address, salt: BytesN<32>) -> Address {
        env.deployer()
            .with_current_contract(scoped_salt(&env, &creator, &salt))
            .deployed_address()
    }

    /// Points future deployments at a new basket WASM. Existing baskets are
    /// unaffected.
    pub fn set_basket_wasm(env: Env, basket_wasm_hash: BytesN<32>) {
        Self::admin(env.clone()).require_auth();
        bump_instance(&env);
        env.storage()
            .instance()
            .set(&DataKey::WasmHash, &basket_wasm_hash);
        env.events()
            .publish((symbol_short!("wasm_set"),), basket_wasm_hash);
    }

    pub fn admin(env: Env) -> Address {
        get_instance(&env, &DataKey::Admin)
    }

    pub fn basket_wasm(env: Env) -> BytesN<32> {
        get_instance(&env, &DataKey::WasmHash)
    }

    pub fn basket_count(env: Env) -> u32 {
        get_instance(&env, &DataKey::Count)
    }

    pub fn is_basket(env: Env, basket: Address) -> bool {
        env.storage().persistent().has(&DataKey::IsBasket(basket))
    }

    /// Registered baskets `[start, start + limit)`, `limit` capped at `MAX_PAGE`.
    pub fn baskets(env: Env, start: u32, limit: u32) -> Vec<Address> {
        let count: u32 = get_instance(&env, &DataKey::Count);
        let end = start.saturating_add(limit.min(MAX_PAGE)).min(count);
        let mut out = Vec::new(&env);
        for id in start..end {
            if let Some(a) = env.storage().persistent().get(&DataKey::Basket(id)) {
                out.push_back(a);
            }
        }
        out
    }
}

fn scoped_salt(env: &Env, creator: &Address, salt: &BytesN<32>) -> BytesN<32> {
    let mut preimage = creator.clone().to_xdr(env);
    preimage.append(&salt.clone().into());
    env.crypto().sha256(&preimage).into()
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

fn get_instance<V: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>(env: &Env, key: &DataKey) -> V {
    env.storage()
        .instance()
        .get(key)
        .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
}

fn set_persistent<V: soroban_sdk::IntoVal<Env, soroban_sdk::Val>>(
    env: &Env,
    key: &DataKey,
    val: &V,
) {
    env.storage().persistent().set(key, val);
    env.storage()
        .persistent()
        .extend_ttl(key, PERSISTENT_THRESHOLD, PERSISTENT_BUMP);
}
