//! Index basket: a SEP-41 token fully collateralised by a fixed set of
//! underlying Soroban tokens.
//!
//! * **Issuance** — a user deposits every component in the basket's current
//!   ratio (rounded up) and receives freshly minted basket tokens.
//! * **Redemption** — a user burns basket tokens and receives their pro-rata
//!   share of every component (rounded down) in one invocation.
//! * **Rebalancing** — when oracle prices move the value weights away from
//!   their targets, authorised arbitrageurs swap an underweight component in
//!   for an overweight one at oracle prices (see [`rebalance`]).
//!
//! Rounding always favours the basket, so the supply is never
//! under-collateralised: for every component `reserve * SHARE >= units_owed`.
//! Deployment is normally done through the `index-basket-factory` crate.

#![no_std]

mod math;
mod oracle;
pub mod rebalance;
mod sep41;
mod storage;

#[cfg(any(test, feature = "testutils"))]
pub mod testutils;

#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, token,
    Address, Env, String, Vec,
};

use math::{mul_div, mul_div_many, pow10, Rounding};
pub use oracle::{Asset, PriceData, PriceOracleClient};

/// Decimals of the basket token itself.
pub const SHARE_DECIMALS: u32 = 7;
/// Base units in one whole basket token.
pub const SHARE: i128 = 10_000_000;
pub const BPS: u32 = 10_000;
pub const MIN_COMPONENTS: u32 = 2;
pub const MAX_COMPONENTS: u32 = 10;
pub const MAX_TOKEN_DECIMALS: u32 = 18;
/// Upper bound on the premium paid to arbitrageurs per rebalance trade.
pub const MAX_INCENTIVE_BPS: u32 = 500;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    /// Component count outside `MIN_COMPONENTS..=MAX_COMPONENTS`.
    InvalidComponents = 2,
    /// A weight is zero or the weights do not sum to `BPS`.
    InvalidWeights = 3,
    DuplicateComponent = 4,
    InvalidParams = 5,
    InvalidAmount = 6,
    /// A trade or redemption rounds down to nothing.
    AmountTooSmall = 7,
    SlippageExceeded = 8,
    LengthMismatch = 9,
    InsufficientBalance = 10,
    InsufficientAllowance = 11,
    InvalidExpiration = 12,
    NotArbitrageur = 13,
    UnknownComponent = 14,
    SameComponent = 15,
    EmptyBasket = 16,
    PriceUnavailable = 17,
    StalePrice = 18,
    /// The pair is not drifted in the direction the trade would correct.
    NotDrifted = 19,
    /// The trade would push a weight past its target by more than the tolerance.
    Overshoot = 20,
    InsufficientReserve = 21,
    Overflow = 22,
}

/// One entry of a basket definition supplied at creation time.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentInit {
    pub token: Address,
    /// Target share of basket value, in basis points.
    pub weight_bps: u32,
}

/// Constructor arguments. The factory mirrors this type field-for-field.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasketParams {
    pub admin: Address,
    pub name: String,
    pub symbol: String,
    pub components: Vec<ComponentInit>,
    /// SEP-40 price oracle quoting every component.
    pub oracle: Address,
    /// Value of one whole basket token at launch, in the oracle's price scale.
    /// Together with the weights and launch prices it fixes the initial units.
    pub initial_nav: i128,
    /// Oldest oracle quote (seconds) accepted for rebalancing.
    pub max_price_age: u64,
    /// Drift band around each target weight, in basis points.
    pub tolerance_bps: u32,
    /// Premium paid to arbitrageurs on rebalance trades, in basis points.
    pub incentive_bps: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Component {
    pub token: Address,
    pub decimals: u32,
    pub weight_bps: u32,
    /// Component base units backing one whole basket token (`SHARE` base
    /// units). Used to price issuance while the supply is zero and refreshed
    /// from reserves after every rebalance.
    pub units: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebalanceConfig {
    pub oracle: Address,
    pub max_price_age: u64,
    pub tolerance_bps: u32,
    pub incentive_bps: u32,
}

#[contract]
pub struct IndexBasket;

#[contractimpl]
impl IndexBasket {
    pub fn __constructor(env: Env, params: BasketParams) {
        let n = params.components.len();
        if !(MIN_COMPONENTS..=MAX_COMPONENTS).contains(&n) {
            panic_with_error!(&env, Error::InvalidComponents);
        }
        if params.initial_nav <= 0
            || params.max_price_age == 0
            || params.tolerance_bps == 0
            || params.incentive_bps > MAX_INCENTIVE_BPS
        {
            panic_with_error!(&env, Error::InvalidParams);
        }

        let cfg = RebalanceConfig {
            oracle: params.oracle.clone(),
            max_price_age: params.max_price_age,
            tolerance_bps: params.tolerance_bps,
            incentive_bps: params.incentive_bps,
        };
        let self_addr = env.current_contract_address();
        let mut weight_sum: u32 = 0;
        let mut components = Vec::new(&env);
        let mut reserves = Vec::new(&env);

        for (i, c) in params.components.iter().enumerate() {
            if c.weight_bps == 0 {
                panic_with_error!(&env, Error::InvalidWeights);
            }
            // The band must leave every component strictly above zero weight
            // so a rebalance can never drain a reserve.
            if params.tolerance_bps >= c.weight_bps {
                panic_with_error!(&env, Error::InvalidParams);
            }
            if c.token == self_addr || c.token == params.oracle {
                panic_with_error!(&env, Error::InvalidParams);
            }
            for prev in params.components.iter().take(i) {
                if prev.token == c.token {
                    panic_with_error!(&env, Error::DuplicateComponent);
                }
            }
            weight_sum = weight_sum
                .checked_add(c.weight_bps)
                .unwrap_or_else(|| panic_with_error!(&env, Error::InvalidWeights));

            let decimals = token::Client::new(&env, &c.token).decimals();
            if decimals > MAX_TOKEN_DECIMALS {
                panic_with_error!(&env, Error::InvalidParams);
            }
            let price = oracle::fresh_price(&env, &cfg, &c.token);
            // units = nav * weight / BPS, converted to base units at `price`.
            let units = mul_div_many(
                &env,
                &[
                    params.initial_nav,
                    c.weight_bps as i128,
                    pow10(&env, decimals),
                ],
                &[BPS as i128, price],
                Rounding::Down,
            );
            if units == 0 {
                panic_with_error!(&env, Error::AmountTooSmall);
            }
            components.push_back(Component {
                token: c.token,
                decimals,
                weight_bps: c.weight_bps,
                units,
            });
            reserves.push_back(0);
        }
        if weight_sum != BPS {
            panic_with_error!(&env, Error::InvalidWeights);
        }

        storage::set_admin(&env, &params.admin);
        storage::set_metadata(
            &env,
            &storage::Metadata {
                name: params.name,
                symbol: params.symbol,
            },
        );
        storage::set_config(&env, &cfg);
        storage::set_components(&env, &components);
        storage::set_reserves(&env, &reserves);
        storage::set_supply(&env, 0);
        storage::extend_instance(&env);
    }

    // ---------------------------------------------------------------------
    // Issuance / redemption
    // ---------------------------------------------------------------------

    /// Mints `amount` basket base units to `to` against an in-kind deposit of
    /// every component. `max_amounts_in` bounds each deposit (index-aligned
    /// with `components`). Returns the amounts pulled.
    pub fn issue(env: Env, to: Address, amount: i128, max_amounts_in: Vec<i128>) -> Vec<i128> {
        to.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        storage::extend_instance(&env);

        let components = storage::components(&env);
        let mut reserves = storage::reserves(&env);
        let supply = storage::supply(&env);
        if max_amounts_in.len() != components.len() {
            panic_with_error!(&env, Error::LengthMismatch);
        }

        let amounts = issue_amounts(&env, &components, &reserves, supply, amount);
        for i in 0..amounts.len() {
            let a = amounts.get_unchecked(i);
            if a > max_amounts_in.get_unchecked(i) {
                panic_with_error!(&env, Error::SlippageExceeded);
            }
            reserves.set(i, checked_add(&env, reserves.get_unchecked(i), a));
        }

        // Effects before interactions.
        storage::set_reserves(&env, &reserves);
        storage::set_supply(&env, checked_add(&env, supply, amount));
        storage::set_balance(
            &env,
            &to,
            checked_add(&env, storage::balance(&env, &to), amount),
        );

        let this = env.current_contract_address();
        for (c, a) in components.iter().zip(amounts.iter()) {
            token::Client::new(&env, &c.token).transfer(&to, &this, &a);
        }

        env.events().publish(
            (symbol_short!("issue"), to.clone()),
            (amount, amounts.clone()),
        );
        env.events().publish((symbol_short!("mint"), to), amount);
        amounts
    }

    /// Burns `amount` basket base units from `from` and releases the pro-rata
    /// share of every component. All transfers happen inside this single
    /// invocation, so redemption either completes for every asset or reverts
    /// as a whole — there is no partially executed state.
    pub fn redeem(env: Env, from: Address, amount: i128, min_amounts_out: Vec<i128>) -> Vec<i128> {
        from.require_auth();
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        storage::extend_instance(&env);

        let components = storage::components(&env);
        let mut reserves = storage::reserves(&env);
        let supply = storage::supply(&env);
        if min_amounts_out.len() != components.len() {
            panic_with_error!(&env, Error::LengthMismatch);
        }
        let balance = storage::balance(&env, &from);
        if balance < amount {
            panic_with_error!(&env, Error::InsufficientBalance);
        }

        let amounts = redeem_amounts(&env, &reserves, supply, amount);
        let mut any = false;
        for i in 0..amounts.len() {
            let a = amounts.get_unchecked(i);
            if a < min_amounts_out.get_unchecked(i) {
                panic_with_error!(&env, Error::SlippageExceeded);
            }
            any |= a > 0;
            reserves.set(i, reserves.get_unchecked(i) - a);
        }
        if !any {
            panic_with_error!(&env, Error::AmountTooSmall);
        }

        storage::set_balance(&env, &from, balance - amount);
        storage::set_supply(&env, supply - amount);
        storage::set_reserves(&env, &reserves);

        let this = env.current_contract_address();
        for (c, a) in components.iter().zip(amounts.iter()) {
            if a > 0 {
                token::Client::new(&env, &c.token).transfer(&this, &from, &a);
            }
        }

        env.events().publish(
            (symbol_short!("redeem"), from.clone()),
            (amount, amounts.clone()),
        );
        env.events().publish((symbol_short!("burn"), from), amount);
        amounts
    }

    /// Deposits `issue(amount)` would pull right now.
    pub fn quote_issue(env: Env, amount: i128) -> Vec<i128> {
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        issue_amounts(
            &env,
            &storage::components(&env),
            &storage::reserves(&env),
            storage::supply(&env),
            amount,
        )
    }

    /// Assets `redeem(amount)` would release right now.
    pub fn quote_redeem(env: Env, amount: i128) -> Vec<i128> {
        let supply = storage::supply(&env);
        if amount <= 0 || amount > supply {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        redeem_amounts(&env, &storage::reserves(&env), supply, amount)
    }

    // ---------------------------------------------------------------------
    // Administration
    // ---------------------------------------------------------------------

    pub fn set_arbitrageur(env: Env, arbitrageur: Address, allowed: bool) {
        storage::admin(&env).require_auth();
        storage::extend_instance(&env);
        storage::set_arbitrageur(&env, &arbitrageur, allowed);
        env.events()
            .publish((symbol_short!("arb_set"), arbitrageur), allowed);
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    pub fn admin(env: Env) -> Address {
        storage::admin(&env)
    }

    pub fn is_arbitrageur(env: Env, id: Address) -> bool {
        storage::is_arbitrageur(&env, &id)
    }

    pub fn components(env: Env) -> Vec<Component> {
        storage::components(&env)
    }

    pub fn reserves(env: Env) -> Vec<i128> {
        storage::reserves(&env)
    }

    pub fn total_supply(env: Env) -> i128 {
        storage::supply(&env)
    }

    pub fn config(env: Env) -> RebalanceConfig {
        storage::config(&env)
    }
}

/// Per-component deposit for minting `amount`, rounded up. While the supply
/// is zero the stored units define the ratio; afterwards the live reserves do,
/// so every new share is backed exactly like the existing ones.
fn issue_amounts(
    env: &Env,
    components: &Vec<Component>,
    reserves: &Vec<i128>,
    supply: i128,
    amount: i128,
) -> Vec<i128> {
    let mut out = Vec::new(env);
    for (c, r) in components.iter().zip(reserves.iter()) {
        out.push_back(if supply == 0 {
            mul_div(env, amount, c.units, SHARE, Rounding::Up)
        } else {
            mul_div(env, amount, r, supply, Rounding::Up)
        });
    }
    out
}

/// Pro-rata release for burning `amount`, rounded down.
fn redeem_amounts(env: &Env, reserves: &Vec<i128>, supply: i128, amount: i128) -> Vec<i128> {
    let mut out = Vec::new(env);
    for r in reserves.iter() {
        out.push_back(mul_div(env, amount, r, supply, Rounding::Down));
    }
    out
}

fn checked_add(env: &Env, a: i128, b: i128) -> i128 {
    a.checked_add(b)
        .unwrap_or_else(|| panic_with_error!(env, Error::Overflow))
}
