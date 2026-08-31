//! # Oracle Aggregator
//!
//! A decentralized oracle aggregator for Stellar (Soroban). Authorized data
//! feeds push prices for assets; the contract aggregates them into a
//! manipulation-resistant median price by trimming statistical outliers and
//! rejecting stale data.
//!
//! ## Design
//!
//! - **Authorized feeds**: only feeds registered by the admin can push prices.
//! - **Medianizer**: the aggregated price is the median, not the mean, so a
//!   single manipulated feed cannot skew the result.
//! - **Outlier trimming**: values whose deviation from the running median
//!   exceeds a configurable threshold (in basis points) are iteratively
//!   dropped before the final median is computed.
//! - **Heartbeat staleness**: price updates older than a configurable window
//!   are rejected at write time and ignored at read time.
//! - **Integer math**: all calculations use fixed-point `u128` arithmetic
//!   with saturating operations, so no floating point and no overflow.

#![no_std]

extern crate alloc;

use alloc::vec::Vec as AllocVec;

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol};

pub mod math;

use math::{is_stale, trimmed_median};

/// Default outlier threshold: 5% (500 basis points).
pub const DEFAULT_THRESHOLD_BPS: u128 = 500;
/// Default heartbeat window: 300 seconds.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 300;
/// Default minimum number of fresh feeds required to produce a price.
pub const DEFAULT_MIN_FEEDS: u32 = 1;

/// Sentinel used to mark a registry slot as an authorized feed.
const REGISTRY_START: u32 = 1;

#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataKey {
    Admin,
    ThresholdBps,
    HeartbeatSecs,
    MinFeeds,
    IsAuthorized(Address),
    Price(Symbol, Address),
    /// Number of entries in the feed registry (persistent counter).
    RegistryLen,
    /// Feed registry entry by index (persistent).
    RegistryEntry(u32),
}

#[contracterror]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    Unauthorized = 3,
    FeedNotAuthorized = 4,
    NotEnoughFeeds = 5,
    StaleData = 6,
    ZeroPrice = 7,
    InvalidThreshold = 8,
    InvalidHeartbeat = 9,
    FeedAlreadyAuthorized = 10,
    NoFreshPrice = 11,
}

#[contract]
pub struct OracleAggregator;

#[contractimpl]
impl OracleAggregator {
    /// Initializes the aggregator.
    ///
    /// - `admin`: the address that may authorize/remove feeds and tune params.
    /// - `threshold_bps`: outlier deviation threshold in basis points
    ///   (default 500 = 5%).
    /// - `heartbeat_secs`: maximum age of a price in seconds (default 300).
    /// - `min_feeds`: minimum number of fresh feeds required to aggregate a
    ///   price (default 1).
    pub fn initialize(
        env: Env,
        admin: Address,
        threshold_bps: Option<u128>,
        heartbeat_secs: Option<u64>,
        min_feeds: Option<u32>,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }

        admin.require_auth();

        let threshold_bps = threshold_bps.unwrap_or(DEFAULT_THRESHOLD_BPS);
        if threshold_bps == 0 || threshold_bps > math::BPS {
            return Err(Error::InvalidThreshold);
        }

        let heartbeat_secs = heartbeat_secs.unwrap_or(DEFAULT_HEARTBEAT_SECS);
        if heartbeat_secs == 0 {
            return Err(Error::InvalidHeartbeat);
        }

        let min_feeds = min_feeds.unwrap_or(DEFAULT_MIN_FEEDS);
        if min_feeds == 0 {
            return Err(Error::InvalidThreshold);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::ThresholdBps, &threshold_bps);
        env.storage()
            .instance()
            .set(&DataKey::HeartbeatSecs, &heartbeat_secs);
        env.storage().instance().set(&DataKey::MinFeeds, &min_feeds);
        env.storage()
            .persistent()
            .set(&DataKey::RegistryLen, &0u32);
        Ok(())
    }

    fn require_initialized(env: &Env) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            Ok(())
        } else {
            Err(Error::NotInitialized)
        }
    }

    fn require_admin(env: &Env) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        admin.require_auth();
        Ok(())
    }

    /// Authorizes `feed` to push prices and appends it to the feed registry.
    /// Admin only.
    pub fn add_feed(env: Env, feed: Address) -> Result<(), Error> {
        Self::require_admin(&env)?;
        if env
            .storage()
            .persistent()
            .has(&DataKey::IsAuthorized(feed.clone()))
        {
            return Err(Error::FeedAlreadyAuthorized);
        }
        env.storage()
            .persistent()
            .set(&DataKey::IsAuthorized(feed.clone()), &true);

        let n: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::RegistryLen)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&DataKey::RegistryEntry(REGISTRY_START + n), &feed);
        env.storage()
            .persistent()
            .set(&DataKey::RegistryLen, &(n + 1));
        Ok(())
    }

    /// Revokes `feed`'s authorization to push prices. Admin only. The feed is
    /// left in the registry so historical prices remain addressable, but it is
    /// skipped during aggregation.
    pub fn remove_feed(env: Env, feed: Address) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.storage()
            .persistent()
            .remove(&DataKey::IsAuthorized(feed.clone()));
        Ok(())
    }

    /// Returns true if `feed` is currently authorized.
    pub fn is_authorized(env: Env, feed: Address) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::IsAuthorized(feed))
    }

    /// Returns the admin address.
    pub fn admin(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)
    }

    /// Pushes a price for `asset` from an authorized feed.
    ///
    /// The feed must be authorized and authenticate the call. `price` is an
    /// integer in the asset's fixed-point units (positive only) and
    /// `timestamp` is the unix time at which the feed observed the price. If
    /// `timestamp` is older than the configured heartbeat the update is
    /// rejected.
    pub fn submit_price(
        env: Env,
        feed: Address,
        asset: Symbol,
        price: u128,
        timestamp: u64,
    ) -> Result<(), Error> {
        Self::require_initialized(&env)?;

        if !Self::is_authorized(env.clone(), feed.clone()) {
            return Err(Error::FeedNotAuthorized);
        }
        feed.require_auth();

        if price == 0 {
            return Err(Error::ZeroPrice);
        }

        let heartbeat_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::HeartbeatSecs)
            .ok_or(Error::NotInitialized)?;
        let now = env.ledger().timestamp();
        if is_stale(timestamp, now, heartbeat_secs) {
            return Err(Error::StaleData);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Price(asset, feed), &(price, timestamp));
        Ok(())
    }

    /// Returns the last price and timestamp pushed by `feed` for `asset`.
    pub fn get_feed_price(env: Env, asset: Symbol, feed: Address) -> Result<(u128, u64), Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Price(asset, feed))
            .ok_or(Error::NoFreshPrice)
    }

    /// Returns the addresses of all feeds currently in the registry.
    pub fn feeds(env: Env) -> soroban_sdk::Vec<Address> {
        let mut out = soroban_sdk::Vec::new(&env);
        let n: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::RegistryLen)
            .unwrap_or(0);
        for i in 0..n {
            if let Some(feed) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::RegistryEntry(REGISTRY_START + i))
            {
                out.push_back(feed);
            }
        }
        out
    }

    /// Aggregates the median price for `asset` across all authorized feeds.
    ///
    /// Stale prices are ignored. The fresh prices are then iteratively
    /// trimmed of outliers before the median is computed.
    pub fn get_price(env: Env, asset: Symbol) -> Result<u128, Error> {
        let heartbeat_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::HeartbeatSecs)
            .ok_or(Error::NotInitialized)?;
        let threshold_bps: u128 = env
            .storage()
            .instance()
            .get(&DataKey::ThresholdBps)
            .ok_or(Error::NotInitialized)?;
        let min_feeds: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MinFeeds)
            .ok_or(Error::NotInitialized)?;

        let now = env.ledger().timestamp();
        let mut prices: AllocVec<u128> = AllocVec::new();

        let n: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::RegistryLen)
            .unwrap_or(0);
        for i in 0..n {
            let feed: Address = env
                .storage()
                .persistent()
                .get(&DataKey::RegistryEntry(REGISTRY_START + i))
                .unwrap();
            if !Self::is_authorized(env.clone(), feed.clone()) {
                continue;
            }
            if let Some((price, timestamp)) = env
                .storage()
                .persistent()
                .get::<DataKey, (u128, u64)>(&DataKey::Price(asset.clone(), feed))
            {
                if !is_stale(timestamp, now, heartbeat_secs) {
                    prices.push(price);
                }
            }
        }

        if (prices.len() as u32) < min_feeds {
            return Err(Error::NotEnoughFeeds);
        }

        trimmed_median(&prices, threshold_bps).ok_or(Error::NotEnoughFeeds)
    }

    /// Returns the configured threshold in basis points.
    pub fn threshold_bps(env: Env) -> Result<u128, Error> {
        env.storage()
            .instance()
            .get(&DataKey::ThresholdBps)
            .ok_or(Error::NotInitialized)
    }

    /// Returns the configured heartbeat window in seconds.
    pub fn heartbeat_secs(env: Env) -> Result<u64, Error> {
        env.storage()
            .instance()
            .get(&DataKey::HeartbeatSecs)
            .ok_or(Error::NotInitialized)
    }

    /// Returns the configured minimum number of feeds.
    pub fn min_feeds(env: Env) -> Result<u32, Error> {
        env.storage()
            .instance()
            .get(&DataKey::MinFeeds)
            .ok_or(Error::NotInitialized)
    }
}
