//! Minimal SEP-40 price feed client (the interface exposed by Reflector and
//! other Stellar oracles).

use soroban_sdk::{contractclient, contracttype, panic_with_error, Address, Env, Symbol};

use crate::{Error, RebalanceConfig};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Asset {
    Stellar(Address),
    Other(Symbol),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

#[allow(dead_code)]
#[contractclient(name = "PriceOracleClient")]
pub trait PriceOracle {
    fn decimals(env: Env) -> u32;
    fn lastprice(env: Env, asset: Asset) -> Option<PriceData>;
}

/// Latest positive price for `token`, rejecting quotes older than
/// `cfg.max_price_age` seconds.
pub fn fresh_price(env: &Env, cfg: &RebalanceConfig, token: &Address) -> i128 {
    let data = PriceOracleClient::new(env, &cfg.oracle)
        .lastprice(&Asset::Stellar(token.clone()))
        .unwrap_or_else(|| panic_with_error!(env, Error::PriceUnavailable));
    if data.price <= 0 {
        panic_with_error!(env, Error::PriceUnavailable);
    }
    if env.ledger().timestamp().saturating_sub(data.timestamp) > cfg.max_price_age {
        panic_with_error!(env, Error::StalePrice);
    }
    data.price
}
