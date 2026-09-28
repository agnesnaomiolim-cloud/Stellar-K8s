//! On-Chain Options Writing (Call/Put) Vault.
//!
//! The vault lets writers escrow collateral and mint standardized, fungible
//! European option tokens for a specific `(underlying, collateral, strike,
//! expiry, kind)` series. Option tokens are fully fungible within a series, so
//! any two "XLM $1.00 call expiring at T" tokens are interchangeable and can be
//! traded like any other asset.
//!
//! ## Guarantees
//!
//! * **Full collateralization.** Writing a call escrows one whole underlying
//!   unit per contract; writing a put escrows `strike` quote units per
//!   contract — enough to cover the maximum possible payoff.
//! * **Isolated collateral.** Every series tracks its own escrow, its own
//!   token supply and its own settlement pools. One series can never touch the
//!   collateral of another.
//! * **Deterministic settlement.** After the exact expiry timestamp the vault
//!   queries the configured oracle once and freezes the settlement price. The
//!   holder payoff pool and the writer residual pool always sum to exactly the
//!   escrowed collateral, so no value is created or stranded.
//! * **Strict expiry.** Options can only be written before expiry and can only
//!   be settled at or after expiry, using the ledger close time.
//! * **Safe under a missing/delayed feed.** Settlement requires an observation
//!   published at or after expiry and no older than `max_staleness`. If the
//!   oracle has not updated yet, `settle` reverts and can simply be retried;
//!   `settlement_ready` lets keepers poll for the open window.
//!
//! The pure payoff/collateral/oracle math lives in [`settlement`].

pub mod settlement;

#[cfg(test)]
mod test;

use settlement::{collateral_required, is_in_the_money, pro_rata_share, settlement_pools};
pub use settlement::{OptionKind, CONTRACT_SIZE, PRICE_SCALE};
use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    token::Client as TokenClient,
    Address, Env,
};

/// Persistent-storage TTL policy: bump entries that are touched so long-lived
/// series survive from listing through settlement and redemption.
const DAY_IN_LEDGERS: u32 = 17_280;
const PERSISTENT_TTL_THRESHOLD: u32 = DAY_IN_LEDGERS;
const PERSISTENT_TTL_EXTEND: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_TTL_THRESHOLD: u32 = DAY_IN_LEDGERS;
const INSTANCE_TTL_EXTEND: u32 = 30 * DAY_IN_LEDGERS;

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    Unauthorized = 3,
    InvalidParameter = 4,
    SeriesNotFound = 5,
    SeriesAlreadyExists = 6,
    SeriesExpired = 7,
    NotExpired = 8,
    AlreadySettled = 9,
    NotSettled = 10,
    OraclePriceUnavailable = 11,
    OraclePriceStale = 12,
    OraclePricePredatesExpiry = 13,
    OraclePriceFromFuture = 14,
    InvalidOraclePrice = 15,
    InsufficientBalance = 16,
    NothingToClaim = 17,
    ArithmeticOverflow = 18,
    KindCollateralMismatch = 19,
}

/// Storage keys.
///
/// A series is identified by the flattened tuple
/// `(kind_code, underlying, collateral, strike, expiry)`. The fields are stored
/// flat rather than nested inside a [`SeriesKey`] struct because Soroban caps a
/// contract-data *ledger key* at 250 bytes, and the nested map encoding of a
/// `SeriesKey` blows past that cap.
///
/// Configuration lives in instance storage; everything that scales with the
/// number of series or holders lives in persistent storage.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    Oracle,
    MaxStaleness,
    /// Immutable metadata + mutable settlement state for one series.
    Series(u32, Address, Address, i128, u64),
    /// Total number of option tokens minted for a series.
    TotalSupply(u32, Address, Address, i128, u64),
    /// Total collateral escrowed for a series.
    TotalCollateral(u32, Address, Address, i128, u64),
    /// Fungible option-token balance of one holder in one series.
    Balance(u32, Address, Address, i128, u64, Address),
    /// Collateral escrowed by one writer in one series.
    WriterCollateral(u32, Address, Address, i128, u64, Address),
    /// Holder payoff pool that has not been redeemed yet.
    RemainingPayout(u32, Address, Address, i128, u64),
    /// Option tokens whose payoff has not been redeemed yet.
    RemainingOptions(u32, Address, Address, i128, u64),
    /// Writer collateral that has not been claimed yet.
    RemainingWriterCollateral(u32, Address, Address, i128, u64),
    /// Residual pool that has not been claimed by writers yet.
    RemainingResidual(u32, Address, Address, i128, u64),
}

/// Identifies a standardized option series. Within a series every option token
/// is fungible; across series they are distinct instruments.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeriesKey {
    pub kind: OptionKind,
    /// Asset whose price the oracle reports (e.g. the XLM token).
    pub underlying: Address,
    /// Token escrowed by writers (the underlying for calls, the quote asset
    /// such as USDC for puts).
    pub collateral: Address,
    /// Strike price in collateral units per whole underlying unit, scaled by
    /// [`PRICE_SCALE`].
    pub strike: i128,
    /// Expiry as a Unix timestamp (ledger close time).
    pub expiry: u64,
}

/// Mutable state for a series.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeriesConfig {
    pub kind: OptionKind,
    pub underlying: Address,
    pub collateral: Address,
    pub strike: i128,
    pub expiry: u64,
    /// Set exactly once, when settlement succeeds.
    pub settled: bool,
    pub settlement_price: i128,
    /// Collateral distributed to option holders (vanilla intrinsic value).
    pub payout_pool: i128,
    /// Collateral returned to writers (escrow minus the payoff).
    pub residual_pool: i128,
}

/// Minimal oracle observation. `price` is scaled by [`PRICE_SCALE`] and is the
/// price of one whole underlying unit in collateral units.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

/// The price feed the vault settles against.
#[contractclient(name = "OracleClient")]
pub trait Oracle {
    /// Latest observation for `asset`, or `None` if the feed has no data.
    fn lastprice(env: Env, asset: Address) -> Option<PriceData>;
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeriesCreated {
    pub kind: OptionKind,
    pub underlying: Address,
    pub collateral: Address,
    pub strike: i128,
    pub expiry: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionWritten {
    pub writer: Address,
    pub kind: OptionKind,
    pub strike: i128,
    pub expiry: u64,
    pub amount: i128,
    pub collateral_locked: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionsTransferred {
    pub from: Address,
    pub to: Address,
    pub kind: OptionKind,
    pub strike: i128,
    pub expiry: u64,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeriesSettled {
    pub kind: OptionKind,
    pub strike: i128,
    pub expiry: u64,
    pub settlement_price: i128,
    pub payout_pool: i128,
    pub residual_pool: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redeemed {
    pub holder: Address,
    pub kind: OptionKind,
    pub strike: i128,
    pub expiry: u64,
    pub payout: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollateralClaimed {
    pub writer: Address,
    pub kind: OptionKind,
    pub strike: i128,
    pub expiry: u64,
    pub amount: i128,
}

#[contract]
pub struct OptionsVault;

#[contractimpl]
impl OptionsVault {
    /// Configure the vault with an admin, the settlement oracle and the
    /// maximum age (seconds) of an oracle observation that may be used to
    /// settle.
    pub fn initialize(
        env: Env,
        admin: Address,
        oracle: Address,
        max_staleness: u64,
    ) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        if max_staleness == 0 {
            return Err(Error::InvalidParameter);
        }
        admin.require_auth();
        let store = env.storage().instance();
        store.set(&DataKey::Admin, &admin);
        store.set(&DataKey::Oracle, &oracle);
        store.set(&DataKey::MaxStaleness, &max_staleness);
        bump_instance(&env);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Admin configuration
    // ------------------------------------------------------------------

    /// Repoint the vault at a different price feed.
    pub fn set_oracle(env: Env, oracle: Address) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::Oracle, &oracle);
        bump_instance(&env);
        Ok(())
    }

    /// Update the maximum accepted age of an oracle observation.
    pub fn set_max_staleness(env: Env, max_staleness: u64) -> Result<(), Error> {
        require_admin(&env)?;
        if max_staleness == 0 {
            return Err(Error::InvalidParameter);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleness, &max_staleness);
        bump_instance(&env);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Series lifecycle
    // ------------------------------------------------------------------

    /// Register a new standardized option series.
    ///
    /// Anyone may list a series; the first listing wins and later attempts
    /// return [`Error::SeriesAlreadyExists`]. Calls must escrow the underlying
    /// itself (`collateral == underlying`) while puts must be cash-collateralized
    /// in a distinct quote asset.
    pub fn create_series(env: Env, creator: Address, key: SeriesKey) -> Result<(), Error> {
        creator.require_auth();
        if key.strike <= 0 {
            return Err(Error::InvalidParameter);
        }
        if key.expiry <= env.ledger().timestamp() {
            return Err(Error::InvalidParameter);
        }
        match key.kind {
            OptionKind::Call => {
                if key.collateral != key.underlying {
                    return Err(Error::KindCollateralMismatch);
                }
            }
            OptionKind::Put => {
                if key.collateral == key.underlying {
                    return Err(Error::KindCollateralMismatch);
                }
            }
        }

        let storage_key = DataKey::series(&key);
        if env.storage().persistent().has(&storage_key) {
            return Err(Error::SeriesAlreadyExists);
        }
        let config = SeriesConfig {
            kind: key.kind,
            underlying: key.underlying.clone(),
            collateral: key.collateral.clone(),
            strike: key.strike,
            expiry: key.expiry,
            settled: false,
            settlement_price: 0,
            payout_pool: 0,
            residual_pool: 0,
        };
        write_config(&env, &key, &config);
        SeriesCreated {
            kind: key.kind,
            underlying: key.underlying,
            collateral: key.collateral,
            strike: key.strike,
            expiry: key.expiry,
        }
        .publish(&env);
        Ok(())
    }

    /// Escrow collateral and mint `amount` fungible option tokens to `writer`.
    ///
    /// Requires a series registered with [`Self::create_series`]. Writing is
    /// only possible strictly before the expiry timestamp.
    pub fn write_option(
        env: Env,
        writer: Address,
        key: SeriesKey,
        amount: i128,
    ) -> Result<(), Error> {
        writer.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidParameter);
        }
        let config = read_config(&env, &key)?;
        if config.settled {
            return Err(Error::AlreadySettled);
        }
        if env.ledger().timestamp() >= config.expiry {
            return Err(Error::SeriesExpired);
        }

        let required = collateral_required(config.kind, config.strike, amount)?;
        TokenClient::new(&env, &config.collateral).transfer(
            &writer,
            &env.current_contract_address(),
            &required,
        );

        add_persistent(&env, &DataKey::total_collateral(&key), required)?;
        add_persistent(&env, &DataKey::writer_collateral(&key, &writer), required)?;
        add_persistent(&env, &DataKey::total_supply(&key), amount)?;
        add_persistent(&env, &DataKey::balance(&key, &writer), amount)?;

        OptionWritten {
            writer,
            kind: config.kind,
            strike: config.strike,
            expiry: config.expiry,
            amount,
            collateral_locked: required,
        }
        .publish(&env);
        Ok(())
    }

    /// Transfer fungible option tokens between holders within a series.
    pub fn transfer(
        env: Env,
        from: Address,
        to: Address,
        key: SeriesKey,
        amount: i128,
    ) -> Result<(), Error> {
        from.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidParameter);
        }
        read_config(&env, &key)?;
        let from_key = DataKey::balance(&key, &from);
        let from_balance = read_persistent::<i128>(&env, &from_key);
        if from_balance < amount {
            return Err(Error::InsufficientBalance);
        }
        write_persistent(&env, &from_key, &(from_balance - amount));
        add_persistent(&env, &DataKey::balance(&key, &to), amount)?;
        OptionsTransferred {
            from,
            to,
            kind: key.kind,
            strike: key.strike,
            expiry: key.expiry,
            amount,
        }
        .publish(&env);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Settlement
    // ------------------------------------------------------------------

    /// Settle a series using the oracle, exactly once, at or after expiry.
    ///
    /// Fails with [`Error::OraclePriceUnavailable`],
    /// [`Error::OraclePricePredatesExpiry`] or [`Error::OraclePriceStale`] when
    /// the feed has not yet published a usable post-expiry observation. In that
    /// case nothing is mutated and the call can safely be retried.
    pub fn settle(env: Env, key: SeriesKey) -> Result<(), Error> {
        let mut config = read_config(&env, &key)?;
        if config.settled {
            return Err(Error::AlreadySettled);
        }
        let now = env.ledger().timestamp();
        if now < config.expiry {
            return Err(Error::NotExpired);
        }

        let (price, timestamp) = observation(&env, &config)?;
        let max_staleness = env
            .storage()
            .instance()
            .get(&DataKey::MaxStaleness)
            .ok_or(Error::NotInitialized)?;
        settlement::validate_oracle_observation(
            price,
            timestamp,
            config.expiry,
            now,
            max_staleness,
        )?;

        let supply = read_persistent::<i128>(&env, &DataKey::total_supply(&key));
        let collateral = read_persistent::<i128>(&env, &DataKey::total_collateral(&key));
        let (payout_pool, residual_pool) = settlement_pools(
            config.kind,
            config.strike,
            price,
            supply,
            collateral,
        )?;

        config.settled = true;
        config.settlement_price = price;
        config.payout_pool = payout_pool;
        config.residual_pool = residual_pool;
        write_config(&env, &key, &config);

        write_persistent(&env, &DataKey::remaining_payout(&key), &payout_pool);
        write_persistent(&env, &DataKey::remaining_options(&key), &supply);
        write_persistent(
            &env,
            &DataKey::remaining_writer_collateral(&key),
            &collateral,
        );
        write_persistent(&env, &DataKey::remaining_residual(&key), &residual_pool);

        SeriesSettled {
            kind: config.kind,
            strike: config.strike,
            expiry: config.expiry,
            settlement_price: price,
            payout_pool,
            residual_pool,
        }
        .publish(&env);
        Ok(())
    }

    /// Redeem an option holder's share of the payoff pool.
    ///
    /// The full option balance is consumed. Out-of-the-money and at-the-money
    /// holders have nothing to redeem.
    pub fn redeem(env: Env, holder: Address, key: SeriesKey) -> Result<i128, Error> {
        holder.require_auth();
        let config = read_config(&env, &key)?;
        if !config.settled {
            return Err(Error::NotSettled);
        }
        let balance_key = DataKey::balance(&key, &holder);
        let balance = read_persistent::<i128>(&env, &balance_key);
        if balance <= 0 {
            return Err(Error::NothingToClaim);
        }
        let remaining_payout = read_persistent::<i128>(&env, &DataKey::remaining_payout(&key));
        if remaining_payout <= 0 {
            return Err(Error::NothingToClaim);
        }
        let remaining_options =
            read_persistent::<i128>(&env, &DataKey::remaining_options(&key));
        let payout = pro_rata_share(balance, remaining_options, remaining_payout);

        write_persistent(&env, &balance_key, &0i128);
        write_persistent(
            &env,
            &DataKey::remaining_options(&key),
            &(remaining_options - balance),
        );
        write_persistent(
            &env,
            &DataKey::remaining_payout(&key),
            &(remaining_payout - payout),
        );

        if payout > 0 {
            TokenClient::new(&env, &config.collateral).transfer(
                &env.current_contract_address(),
                &holder,
                &payout,
            );
        }
        Redeemed {
            holder,
            kind: config.kind,
            strike: config.strike,
            expiry: config.expiry,
            payout,
        }
        .publish(&env);
        Ok(payout)
    }

    /// Claim a writer's residual collateral after settlement.
    ///
    /// In-the-money writers receive the escrow left over after the holder
    /// payoff; out-of-the-money and at-the-money writers receive their full
    /// escrow back.
    pub fn claim_collateral(env: Env, writer: Address, key: SeriesKey) -> Result<i128, Error> {
        writer.require_auth();
        let config = read_config(&env, &key)?;
        if !config.settled {
            return Err(Error::NotSettled);
        }
        let writer_key = DataKey::writer_collateral(&key, &writer);
        let writer_collateral = read_persistent::<i128>(&env, &writer_key);
        if writer_collateral <= 0 {
            return Err(Error::NothingToClaim);
        }
        let remaining_residual = read_persistent::<i128>(&env, &DataKey::remaining_residual(&key));
        if remaining_residual <= 0 {
            return Err(Error::NothingToClaim);
        }
        let remaining_writer_collateral =
            read_persistent::<i128>(&env, &DataKey::remaining_writer_collateral(&key));
        let amount =
            pro_rata_share(writer_collateral, remaining_writer_collateral, remaining_residual);

        write_persistent(&env, &writer_key, &0i128);
        write_persistent(
            &env,
            &DataKey::remaining_writer_collateral(&key),
            &(remaining_writer_collateral - writer_collateral),
        );
        write_persistent(
            &env,
            &DataKey::remaining_residual(&key),
            &(remaining_residual - amount),
        );

        if amount > 0 {
            TokenClient::new(&env, &config.collateral).transfer(
                &env.current_contract_address(),
                &writer,
                &amount,
            );
        }
        CollateralClaimed {
            writer,
            kind: config.kind,
            strike: config.strike,
            expiry: config.expiry,
            amount,
        }
        .publish(&env);
        Ok(amount)
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    pub fn admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn oracle(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Oracle).unwrap()
    }

    pub fn max_staleness(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MaxStaleness)
            .unwrap()
    }

    pub fn get_series(env: Env, key: SeriesKey) -> Option<SeriesConfig> {
        env.storage().persistent().get(&DataKey::series(&key))
    }

    pub fn total_supply(env: Env, key: SeriesKey) -> i128 {
        read_persistent::<i128>(&env, &DataKey::total_supply(&key))
    }

    pub fn balance_of(env: Env, key: SeriesKey, holder: Address) -> i128 {
        read_persistent::<i128>(&env, &DataKey::balance(&key, &holder))
    }

    pub fn total_collateral(env: Env, key: SeriesKey) -> i128 {
        read_persistent::<i128>(&env, &DataKey::total_collateral(&key))
    }

    pub fn writer_collateral(env: Env, key: SeriesKey, writer: Address) -> i128 {
        read_persistent::<i128>(&env, &DataKey::writer_collateral(&key, &writer))
    }

    /// Whether [`Self::settle`] could succeed right now: the series is past
    /// expiry, unsettled, and the oracle currently reports a usable
    /// observation. Keepers can poll this after expiry instead of retrying
    /// `settle` blindly while the feed catches up.
    pub fn settlement_ready(env: Env, key: SeriesKey) -> bool {
        let config = match read_config(&env, &key) {
            Ok(config) => config,
            Err(_) => return false,
        };
        if config.settled {
            return false;
        }
        let now = env.ledger().timestamp();
        if now < config.expiry {
            return false;
        }
        let (price, timestamp) = match observation(&env, &config) {
            Ok(observation) => observation,
            Err(_) => return false,
        };
        let max_staleness = match env.storage().instance().get(&DataKey::MaxStaleness) {
            Some(value) => value,
            None => return false,
        };
        settlement::validate_oracle_observation(
            price,
            timestamp,
            config.expiry,
            now,
            max_staleness,
        )
        .is_ok()
    }

    /// Whether a settled series ended up in the money.
    pub fn in_the_money(env: Env, key: SeriesKey) -> bool {
        match read_config(&env, &key) {
            Ok(config) if config.settled => {
                is_in_the_money(config.kind, config.strike, config.settlement_price)
            }
            _ => false,
        }
    }
}

impl DataKey {
    fn series(key: &SeriesKey) -> DataKey {
        DataKey::Series(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
        )
    }

    fn total_supply(key: &SeriesKey) -> DataKey {
        DataKey::TotalSupply(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
        )
    }

    fn total_collateral(key: &SeriesKey) -> DataKey {
        DataKey::TotalCollateral(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
        )
    }

    fn balance(key: &SeriesKey, holder: &Address) -> DataKey {
        DataKey::Balance(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
            holder.clone(),
        )
    }

    fn writer_collateral(key: &SeriesKey, writer: &Address) -> DataKey {
        DataKey::WriterCollateral(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
            writer.clone(),
        )
    }

    fn remaining_payout(key: &SeriesKey) -> DataKey {
        DataKey::RemainingPayout(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
        )
    }

    fn remaining_options(key: &SeriesKey) -> DataKey {
        DataKey::RemainingOptions(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
        )
    }

    fn remaining_writer_collateral(key: &SeriesKey) -> DataKey {
        DataKey::RemainingWriterCollateral(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
        )
    }

    fn remaining_residual(key: &SeriesKey) -> DataKey {
        DataKey::RemainingResidual(
            kind_code(key.kind),
            key.underlying.clone(),
            key.collateral.clone(),
            key.strike,
            key.expiry,
        )
    }
}

/// Compact on-chain discriminant for an option kind, used inside ledger keys.
fn kind_code(kind: OptionKind) -> u32 {
    match kind {
        OptionKind::Call => 0,
        OptionKind::Put => 1,
    }
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND);
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

/// Fetch and unwrap a single oracle observation.
fn observation(env: &Env, config: &SeriesConfig) -> Result<(i128, u64), Error> {
    let oracle: Address = env
        .storage()
        .instance()
        .get(&DataKey::Oracle)
        .ok_or(Error::NotInitialized)?;
    let observation = OracleClient::new(env, &oracle)
        .lastprice(&config.underlying)
        .ok_or(Error::OraclePriceUnavailable)?;
    Ok((observation.price, observation.timestamp))
}

fn read_config(env: &Env, key: &SeriesKey) -> Result<SeriesConfig, Error> {
    let data_key = DataKey::series(key);
    let store = env.storage().persistent();
    let config = store
        .get::<DataKey, SeriesConfig>(&data_key)
        .ok_or(Error::SeriesNotFound)?;
    store.extend_ttl(&data_key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
    Ok(config)
}

fn write_config(env: &Env, key: &SeriesKey, config: &SeriesConfig) {
    let data_key = DataKey::series(key);
    let store = env.storage().persistent();
    store.set(&data_key, config);
    store.extend_ttl(&data_key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
}

fn read_persistent<T>(env: &Env, key: &DataKey) -> T
where
    T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val> + Default,
{
    let store = env.storage().persistent();
    if !store.has(key) {
        return T::default();
    }
    store.extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
    store.get::<DataKey, T>(key).unwrap_or_default()
}

fn write_persistent<T>(env: &Env, key: &DataKey, value: &T)
where
    T: soroban_sdk::IntoVal<Env, soroban_sdk::Val>,
{
    let store = env.storage().persistent();
    store.set(key, value);
    store.extend_ttl(key, PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND);
}

fn add_persistent(env: &Env, key: &DataKey, delta: i128) -> Result<(), Error> {
    let current = read_persistent::<i128>(env, key);
    let next = current.checked_add(delta).ok_or(Error::ArithmeticOverflow)?;
    write_persistent(env, key, &next);
    Ok(())
}
