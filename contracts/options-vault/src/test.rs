#![cfg(test)]

//! End-to-end tests for the options vault, driven through the generated
//! contract clients on a real Soroban host environment.

use super::*;
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

const STRIKE: i128 = PRICE_SCALE; // $1.00 per whole underlying unit
const EXPIRY: u64 = 1_000_000;
const MAX_STALENESS: u64 = 300;

/// A minimal price feed used to drive settlement in the tests. It returns
/// whatever single observation was last written (or `None` when cleared),
/// which lets tests exercise the missing/delayed-feed paths precisely.
#[contract]
pub struct MockOracle;

#[contractimpl]
impl MockOracle {
    pub fn set(env: Env, price: i128, timestamp: u64) {
        env.storage()
            .instance()
            .set(&symbol_short!("price"), &PriceData { price, timestamp });
    }

    pub fn clear(env: Env) {
        env.storage().instance().remove(&symbol_short!("price"));
    }

    pub fn lastprice(env: Env, _asset: Address) -> Option<PriceData> {
        env.storage().instance().get(&symbol_short!("price"))
    }
}

struct Fixture {
    env: Env,
    vault: OptionsVaultClient<'static>,
    vault_id: Address,
    oracle: Address,
    underlying: Address,
    quote: Address,
    admin: Address,
    writer: Address,
    holder: Address,
}

impl Fixture {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let oracle = env.register(MockOracle, ());
        let vault_id = env.register(OptionsVault, ());
        let vault = OptionsVaultClient::new(&env, &vault_id);
        vault.initialize(&admin, &oracle, &MAX_STALENESS);

        // Two distinct Stellar Asset Contracts: the underlying being priced
        // (e.g. XLM) and the quote collateral (e.g. USDC).
        let underlying = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();
        let quote = env
            .register_stellar_asset_contract_v2(Address::generate(&env))
            .address();

        let writer = Address::generate(&env);
        let holder = Address::generate(&env);

        env.ledger().set_timestamp(EXPIRY - 10_000);
        Self {
            env,
            vault,
            vault_id,
            oracle,
            underlying,
            quote,
            admin,
            writer,
            holder,
        }
    }

    fn call_key(&self) -> SeriesKey {
        SeriesKey {
            kind: OptionKind::Call,
            underlying: self.underlying.clone(),
            collateral: self.underlying.clone(),
            strike: STRIKE,
            expiry: EXPIRY,
        }
    }

    fn put_key(&self) -> SeriesKey {
        SeriesKey {
            kind: OptionKind::Put,
            underlying: self.underlying.clone(),
            collateral: self.quote.clone(),
            strike: STRIKE,
            expiry: EXPIRY,
        }
    }

    fn mint(&self, token: &Address, to: &Address, amount: i128) {
        StellarAssetClient::new(&self.env, token).mint(to, &amount);
    }

    fn token_balance(&self, token: &Address, who: &Address) -> i128 {
        TokenClient::new(&self.env, token).balance(who)
    }

    fn set_price(&self, price: i128, timestamp: u64) {
        MockOracleClient::new(&self.env, &self.oracle).set(&price, &timestamp);
    }

    fn clear_price(&self) {
        MockOracleClient::new(&self.env, &self.oracle).clear();
    }

    /// Writes `amount` call contracts and hands them to `holder`.
    fn write_call_to_holder(&self, amount: i128) {
        self.vault.create_series(&self.writer, &self.call_key());
        self.mint(&self.underlying, &self.writer, amount * CONTRACT_SIZE);
        self.vault
            .write_option(&self.writer, &self.call_key(), &amount);
        self.vault
            .transfer(&self.writer, &self.holder, &self.call_key(), &amount);
    }
}

// --------------------------------------------------------------------------
// Series registration
// --------------------------------------------------------------------------

#[test]
fn create_series_stores_metadata() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.vault.create_series(&fx.writer, &key);

    let config = fx.vault.get_series(&key).unwrap();
    assert_eq!(config.kind, OptionKind::Call);
    assert_eq!(config.strike, STRIKE);
    assert_eq!(config.expiry, EXPIRY);
    assert!(!config.settled);
    assert_eq!(config.settlement_price, 0);
}

#[test]
fn create_series_rejects_invalid_parameters() {
    let fx = Fixture::new();

    let mut zero_strike = fx.call_key();
    zero_strike.strike = 0;
    assert_eq!(
        fx.vault.try_create_series(&fx.writer, &zero_strike),
        Err(Ok(Error::InvalidParameter))
    );

    let mut past_expiry = fx.call_key();
    past_expiry.expiry = fx.env.ledger().timestamp();
    assert_eq!(
        fx.vault.try_create_series(&fx.writer, &past_expiry),
        Err(Ok(Error::InvalidParameter))
    );

    // A call must escrow the underlying itself.
    let mut bad_call = fx.call_key();
    bad_call.collateral = fx.quote.clone();
    assert_eq!(
        fx.vault.try_create_series(&fx.writer, &bad_call),
        Err(Ok(Error::KindCollateralMismatch))
    );

    // A put must escrow a distinct quote asset.
    let mut bad_put = fx.put_key();
    bad_put.collateral = fx.underlying.clone();
    assert_eq!(
        fx.vault.try_create_series(&fx.writer, &bad_put),
        Err(Ok(Error::KindCollateralMismatch))
    );
}

#[test]
fn create_series_is_idempotent_once_registered() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.vault.create_series(&fx.writer, &key);
    assert_eq!(
        fx.vault.try_create_series(&fx.holder, &key),
        Err(Ok(Error::SeriesAlreadyExists))
    );
}

// --------------------------------------------------------------------------
// Writing and transferring option tokens
// --------------------------------------------------------------------------

#[test]
fn write_option_escrows_collateral_and_mints_tokens() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.vault.create_series(&fx.writer, &key);
    fx.mint(&fx.underlying, &fx.writer, 10 * CONTRACT_SIZE);

    fx.vault.write_option(&fx.writer, &key, &10);

    // One whole underlying unit is locked per contract.
    assert_eq!(fx.vault.total_collateral(&key), 10 * CONTRACT_SIZE);
    assert_eq!(
        fx.vault.writer_collateral(&key, &fx.writer),
        10 * CONTRACT_SIZE
    );
    assert_eq!(fx.vault.total_supply(&key), 10);
    assert_eq!(fx.vault.balance_of(&key, &fx.writer), 10);
    assert_eq!(
        fx.token_balance(&fx.underlying, &fx.vault_id),
        10 * CONTRACT_SIZE
    );
    assert_eq!(fx.token_balance(&fx.underlying, &fx.writer), 0);
}

#[test]
fn put_collateral_is_strike_denominated() {
    let fx = Fixture::new();
    let key = fx.put_key();
    fx.vault.create_series(&fx.writer, &key);
    fx.mint(&fx.quote, &fx.writer, 10 * STRIKE);

    fx.vault.write_option(&fx.writer, &key, &10);
    assert_eq!(fx.vault.total_collateral(&key), 10 * STRIKE);
}

#[test]
fn write_option_rejected_after_expiry() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.vault.create_series(&fx.writer, &key);
    fx.mint(&fx.underlying, &fx.writer, 10 * CONTRACT_SIZE);

    fx.env.ledger().set_timestamp(EXPIRY);
    assert_eq!(
        fx.vault.try_write_option(&fx.writer, &key, &10),
        Err(Ok(Error::SeriesExpired))
    );
}

#[test]
fn transfer_moves_fungible_tokens() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.vault.create_series(&fx.writer, &key);
    fx.mint(&fx.underlying, &fx.writer, 10 * CONTRACT_SIZE);
    fx.vault.write_option(&fx.writer, &key, &10);

    fx.vault.transfer(&fx.writer, &fx.holder, &key, &4);
    assert_eq!(fx.vault.balance_of(&key, &fx.writer), 6);
    assert_eq!(fx.vault.balance_of(&key, &fx.holder), 4);
    // Supply is unchanged by a transfer.
    assert_eq!(fx.vault.total_supply(&key), 10);

    assert_eq!(
        fx.vault.try_transfer(&fx.writer, &fx.holder, &key, &7),
        Err(Ok(Error::InsufficientBalance))
    );
}

// --------------------------------------------------------------------------
// Settlement timing and oracle edge cases
// --------------------------------------------------------------------------

#[test]
fn settle_is_rejected_before_expiry() {
    let fx = Fixture::new();
    fx.write_call_to_holder(10);
    fx.set_price(STRIKE * 2, EXPIRY);

    fx.env.ledger().set_timestamp(EXPIRY - 1);
    assert_eq!(
        fx.vault.try_settle(&fx.call_key()),
        Err(Ok(Error::NotExpired))
    );
    assert!(!fx.vault.get_series(&fx.call_key()).unwrap().settled);
}

#[test]
fn settle_rejects_missing_oracle_observation() {
    let fx = Fixture::new();
    fx.write_call_to_holder(10);
    fx.env.ledger().set_timestamp(EXPIRY);
    fx.clear_price();

    assert_eq!(
        fx.vault.try_settle(&fx.call_key()),
        Err(Ok(Error::OraclePriceUnavailable))
    );
    assert!(!fx.vault.get_series(&fx.call_key()).unwrap().settled);
    assert!(!fx.vault.settlement_ready(&fx.call_key()));
}

#[test]
fn settle_waits_for_a_post_expiry_observation() {
    let fx = Fixture::new();
    fx.write_call_to_holder(10);
    let key = fx.call_key();

    // At expiry the feed is still publishing its last pre-expiry price: a
    // keeper must not settle on it.
    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE * 2, EXPIRY - 1);
    assert_eq!(
        fx.vault.try_settle(&key),
        Err(Ok(Error::OraclePricePredatesExpiry))
    );
    assert!(!fx.vault.settlement_ready(&key));

    // Once the feed publishes a post-expiry observation settlement proceeds.
    fx.set_price(STRIKE * 2, EXPIRY);
    assert!(fx.vault.settlement_ready(&key));
    fx.vault.settle(&key);
    assert!(fx.vault.get_series(&key).unwrap().settled);
}

#[test]
fn settle_rejects_stale_and_future_observations() {
    let fx = Fixture::new();
    fx.write_call_to_holder(10);
    let key = fx.call_key();

    // Post-expiry observation that is older than `max_staleness`.
    fx.env.ledger().set_timestamp(EXPIRY + MAX_STALENESS + 1);
    fx.set_price(STRIKE * 2, EXPIRY);
    assert_eq!(
        fx.vault.try_settle(&key),
        Err(Ok(Error::OraclePriceStale))
    );

    // Observation stamped in the future.
    fx.set_price(STRIKE * 2, fx.env.ledger().timestamp() + 1);
    assert_eq!(
        fx.vault.try_settle(&key),
        Err(Ok(Error::OraclePriceFromFuture))
    );

    // A fresh observation is accepted and settlement succeeds.
    fx.set_price(STRIKE * 2, fx.env.ledger().timestamp());
    fx.vault.settle(&key);
}

#[test]
fn settle_rejects_non_positive_price() {
    let fx = Fixture::new();
    fx.write_call_to_holder(10);
    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(0, EXPIRY);
    assert_eq!(
        fx.vault.try_settle(&fx.call_key()),
        Err(Ok(Error::InvalidOraclePrice))
    );

    fx.set_price(-1, EXPIRY);
    assert_eq!(
        fx.vault.try_settle(&fx.call_key()),
        Err(Ok(Error::InvalidOraclePrice))
    );
}

#[test]
fn settle_cannot_run_twice() {
    let fx = Fixture::new();
    fx.write_call_to_holder(10);
    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE * 2, EXPIRY);
    fx.vault.settle(&fx.call_key());
    assert_eq!(
        fx.vault.try_settle(&fx.call_key()),
        Err(Ok(Error::AlreadySettled))
    );
}

// --------------------------------------------------------------------------
// Payoff distribution
// --------------------------------------------------------------------------

#[test]
fn call_in_the_money_splits_escrow_between_holders_and_writers() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.write_call_to_holder(10); // locks 10 whole underlying units

    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE * 3 / 2, EXPIRY); // $1.50
    fx.vault.settle(&key);

    let config = fx.vault.get_series(&key).unwrap();
    assert!(fx.vault.in_the_money(&key));
    assert_eq!(config.settlement_price, STRIKE * 3 / 2);
    assert_eq!(config.payout_pool, 33_333_333);
    assert_eq!(config.residual_pool, 10 * CONTRACT_SIZE - 33_333_333);

    // The holder redeems the whole payoff pool (they own the whole supply).
    let payout = fx.vault.redeem(&fx.holder, &key);
    assert_eq!(payout, 33_333_333);
    assert_eq!(fx.vault.balance_of(&key, &fx.holder), 0);
    assert_eq!(fx.token_balance(&fx.underlying, &fx.holder), 33_333_333);

    // The writer reclaims the residual.
    let residual = fx.vault.claim_collateral(&fx.writer, &key);
    assert_eq!(residual, 10 * CONTRACT_SIZE - 33_333_333);
    assert_eq!(fx.vault.writer_collateral(&key, &fx.writer), 0);
    assert_eq!(fx.token_balance(&fx.underlying, &fx.writer), residual);

    // The escrow is drained exactly: no value created, none stranded.
    assert_eq!(fx.token_balance(&fx.underlying, &fx.vault_id), 0);
    assert_eq!(payout + residual, 10 * CONTRACT_SIZE);
}

#[test]
fn call_out_of_the_money_returns_all_collateral_to_writer() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.write_call_to_holder(10);

    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE / 2, EXPIRY); // below strike -> worthless
    fx.vault.settle(&key);

    let config = fx.vault.get_series(&key).unwrap();
    assert!(!fx.vault.in_the_money(&key));
    assert_eq!(config.payout_pool, 0);

    // The holder has nothing to redeem.
    assert_eq!(
        fx.vault.try_redeem(&fx.holder, &key),
        Err(Ok(Error::NothingToClaim))
    );

    // The writer gets every unit of escrow back.
    let residual = fx.vault.claim_collateral(&fx.writer, &key);
    assert_eq!(residual, 10 * CONTRACT_SIZE);
    assert_eq!(
        fx.token_balance(&fx.underlying, &fx.writer),
        10 * CONTRACT_SIZE
    );
    assert_eq!(fx.token_balance(&fx.underlying, &fx.vault_id), 0);
}

#[test]
fn put_in_the_money_pays_intrinsic_value() {
    let fx = Fixture::new();
    let key = fx.put_key();
    fx.vault.create_series(&fx.writer, &key);
    fx.mint(&fx.quote, &fx.writer, 10 * STRIKE);
    fx.vault.write_option(&fx.writer, &key, &10);
    fx.vault.transfer(&fx.writer, &fx.holder, &key, &10);

    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE / 2, EXPIRY); // $0.50 vs $1.00 strike
    fx.vault.settle(&key);

    let config = fx.vault.get_series(&key).unwrap();
    assert_eq!(config.payout_pool, 50_000_000);
    assert_eq!(config.residual_pool, 50_000_000);

    assert_eq!(fx.vault.redeem(&fx.holder, &key), 50_000_000);
    assert_eq!(fx.vault.claim_collateral(&fx.writer, &key), 50_000_000);
    assert_eq!(fx.token_balance(&fx.quote, &fx.vault_id), 0);
}

#[test]
fn exactly_at_the_strike_expires_worthless() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.write_call_to_holder(10);

    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE, EXPIRY); // exactly at the strike
    fx.vault.settle(&key);

    assert!(!fx.vault.in_the_money(&key));
    assert_eq!(
        fx.vault.try_redeem(&fx.holder, &key),
        Err(Ok(Error::NothingToClaim))
    );
    assert_eq!(
        fx.vault.claim_collateral(&fx.writer, &key),
        10 * CONTRACT_SIZE
    );
}

#[test]
fn claims_rejected_before_settlement() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.write_call_to_holder(10);

    assert_eq!(
        fx.vault.try_redeem(&fx.holder, &key),
        Err(Ok(Error::NotSettled))
    );
    assert_eq!(
        fx.vault.try_claim_collateral(&fx.writer, &key),
        Err(Ok(Error::NotSettled))
    );
}

#[test]
fn double_claims_are_rejected() {
    let fx = Fixture::new();
    let key = fx.call_key();
    fx.write_call_to_holder(10);
    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE * 2, EXPIRY);
    fx.vault.settle(&key);

    fx.vault.redeem(&fx.holder, &key);
    assert_eq!(
        fx.vault.try_redeem(&fx.holder, &key),
        Err(Ok(Error::NothingToClaim))
    );
    fx.vault.claim_collateral(&fx.writer, &key);
    assert_eq!(
        fx.vault.try_claim_collateral(&fx.writer, &key),
        Err(Ok(Error::NothingToClaim))
    );
}

#[test]
fn multiple_holders_split_the_payoff_pro_rata() {
    let fx = Fixture::new();
    let key = fx.put_key();
    fx.vault.create_series(&fx.writer, &key);
    // 100 contracts -> 100 * STRIKE quote collateral.
    fx.mint(&fx.quote, &fx.writer, 100 * STRIKE);
    fx.vault.write_option(&fx.writer, &key, &100);

    let alice = Address::generate(&fx.env);
    let bob = Address::generate(&fx.env);
    fx.vault.transfer(&fx.writer, &alice, &key, &30);
    fx.vault.transfer(&fx.writer, &bob, &key, &20);
    // The writer keeps 50.

    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE / 2, EXPIRY); // payoff = STRIKE/2 per contract
    fx.vault.settle(&key);

    // Total payoff pool = 100 * (STRIKE - STRIKE/2) = 50 * STRIKE.
    assert_eq!(fx.vault.redeem(&alice, &key), 30 * (STRIKE / 2));
    assert_eq!(fx.vault.redeem(&bob, &key), 20 * (STRIKE / 2));
    assert_eq!(fx.vault.redeem(&fx.writer, &key), 50 * (STRIKE / 2));

    // Only the writers' residual remains escrowed at this point.
    assert_eq!(fx.token_balance(&fx.quote, &fx.vault_id), 50 * STRIKE);
}

#[test]
fn collateral_is_isolated_between_series() {
    let fx = Fixture::new();
    let call_key = fx.call_key();
    let put_key = fx.put_key();
    fx.vault.create_series(&fx.writer, &call_key);
    fx.vault.create_series(&fx.writer, &put_key);

    fx.mint(&fx.underlying, &fx.writer, 5 * CONTRACT_SIZE);
    fx.mint(&fx.quote, &fx.writer, 5 * STRIKE);
    fx.vault.write_option(&fx.writer, &call_key, &5);
    fx.vault.write_option(&fx.writer, &put_key, &5);

    assert_eq!(fx.vault.total_collateral(&call_key), 5 * CONTRACT_SIZE);
    assert_eq!(fx.vault.total_collateral(&put_key), 5 * STRIKE);
    // Balances are per-series too.
    assert_eq!(fx.vault.balance_of(&call_key, &fx.writer), 5);
    assert_eq!(fx.vault.balance_of(&put_key, &fx.writer), 5);

    // Settling the worthless call returns its escrow without touching the put.
    fx.env.ledger().set_timestamp(EXPIRY);
    fx.set_price(STRIKE / 2, EXPIRY);
    fx.vault.settle(&call_key);
    fx.vault.claim_collateral(&fx.writer, &call_key);
    assert_eq!(fx.vault.total_collateral(&call_key), 5 * CONTRACT_SIZE);
    assert_eq!(fx.vault.total_collateral(&put_key), 5 * STRIKE);
    assert!(!fx.vault.get_series(&put_key).unwrap().settled);
}

// --------------------------------------------------------------------------
// Admin configuration
// --------------------------------------------------------------------------

#[test]
fn admin_can_update_oracle_and_staleness() {
    let fx = Fixture::new();
    assert_eq!(fx.vault.admin(), fx.admin);
    assert_eq!(fx.vault.oracle(), fx.oracle);
    assert_eq!(fx.vault.max_staleness(), MAX_STALENESS);

    let new_oracle = Address::generate(&fx.env);
    fx.vault.set_oracle(&new_oracle);
    assert_eq!(fx.vault.oracle(), new_oracle);

    fx.vault.set_max_staleness(&60);
    assert_eq!(fx.vault.max_staleness(), 60);

    assert_eq!(
        fx.vault.try_set_max_staleness(&0),
        Err(Ok(Error::InvalidParameter))
    );
}
