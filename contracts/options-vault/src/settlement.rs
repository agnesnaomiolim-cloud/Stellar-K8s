//! Deterministic settlement math for the options vault.
//!
//! Every function in this module is *pure* (it takes no [`soroban_sdk::Env`]),
//! which keeps the payoff rules, the collateral requirement and the oracle
//! freshness policy unit-testable in isolation and free of storage or host
//! side effects.
//!
//! ## Units and precision
//!
//! Stellar assets use 7 decimals, so the vault reuses that precision
//! throughout:
//!
//! * [`PRICE_SCALE`] is the fixed-point scale for oracle prices and strikes.
//!   A price of `1.5 * PRICE_SCALE` means "1.5 collateral units per *whole*
//!   underlying unit".
//! * A single option contract covers [`CONTRACT_SIZE`] raw underlying units
//!   (i.e. one whole 7-decimal unit).
//!
//! Both constants are numerically `10^7`, which keeps the arithmetic free of
//! hidden scaling factors.

use soroban_sdk::contracttype;

use crate::Error;

/// Fixed-point scale for prices and strikes (Stellar's 7-decimal precision).
pub const PRICE_SCALE: i128 = 10_000_000;

/// Raw underlying units covered by a single option contract (one whole unit).
pub const CONTRACT_SIZE: i128 = 10_000_000;

/// European option flavour.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionKind {
    /// Right to buy the underlying at `strike`. The writer escrows the
    /// underlying itself (e.g. XLM), which fully covers physical delivery.
    Call,
    /// Right to sell the underlying at `strike`. The writer escrows the quote
    /// asset (e.g. USDC), which fully covers the maximum cash payoff.
    Put,
}

/// Returns the amount of collateral that must be escrowed to write `amount`
/// contracts of a series.
///
/// * `Call` — `amount * CONTRACT_SIZE` units of the underlying. One whole
///   underlying unit is locked per contract, so the vault can always deliver.
/// * `Put`  — `amount * strike` units of the quote collateral. `strike` is the
///   largest possible per-contract payoff (when the underlying goes to zero),
///   so this fully collateralizes the short put.
pub fn collateral_required(kind: OptionKind, strike: i128, amount: i128) -> Result<i128, Error> {
    if amount <= 0 || strike <= 0 {
        return Err(Error::InvalidParameter);
    }
    let per_contract = match kind {
        OptionKind::Call => CONTRACT_SIZE,
        OptionKind::Put => strike,
    };
    amount
        .checked_mul(per_contract)
        .ok_or(Error::ArithmeticOverflow)
}

/// Whether `price` (scaled by [`PRICE_SCALE`]) makes the option in-the-money.
///
/// Touching the strike exactly is *at-the-money* and settles like an
/// out-of-the-money option: it expires worthless, so the writer keeps the
/// collateral. This is the boundary the specification calls out explicitly.
pub fn is_in_the_money(kind: OptionKind, strike: i128, price: i128) -> bool {
    match kind {
        OptionKind::Call => price > strike,
        OptionKind::Put => price < strike,
    }
}

/// Intrinsic payoff, in collateral units, owed to the holders of `supply`
/// contracts given a settlement `price`.
///
/// * `Put`  — `supply * (strike - price)`, in quote collateral.
/// * `Call` — `supply * (price - strike) * CONTRACT_SIZE / price`, i.e. the
///   quote-currency intrinsic value converted into underlying collateral at
///   the settlement price.
///
/// The result is capped at `collateral` (the amount actually escrowed) so the
/// vault can never promise more than it holds.
pub fn payoff_pool(
    kind: OptionKind,
    strike: i128,
    price: i128,
    supply: i128,
    collateral: i128,
) -> Result<i128, Error> {
    if strike <= 0 || price <= 0 || supply < 0 || collateral < 0 {
        return Err(Error::InvalidOraclePrice);
    }
    if supply == 0 || !is_in_the_money(kind, strike, price) {
        return Ok(0);
    }
    let payout = match kind {
        OptionKind::Put => supply
            .checked_mul(strike - price)
            .ok_or(Error::ArithmeticOverflow)?,
        OptionKind::Call => {
            let numerator = supply
                .checked_mul(price - strike)
                .and_then(|v| v.checked_mul(CONTRACT_SIZE))
                .ok_or(Error::ArithmeticOverflow)?;
            numerator / price
        }
    };
    Ok(if payout > collateral { collateral } else { payout })
}

/// Splits the escrow into the holder payoff pool and the writer residual pool.
///
/// The two pools always sum to exactly `collateral`, so the vault is solvency
/// invariant: nothing is created and nothing is stranded.
pub fn settlement_pools(
    kind: OptionKind,
    strike: i128,
    price: i128,
    supply: i128,
    collateral: i128,
) -> Result<(i128, i128), Error> {
    let payout = payoff_pool(kind, strike, price, supply, collateral)?;
    Ok((payout, collateral - payout))
}

/// Validates an oracle observation for settlement.
///
/// Settlement must be driven by an observation that was published *at or after*
/// the series expiry (so a delayed feed cannot settle on a pre-expiry price)
/// and that is no older than `max_staleness` seconds. Callers that find the
/// observation unusable can simply retry later — `settle` is idempotent until
/// it succeeds, which is exactly the "missing or delayed oracle update at the
/// moment of expiry" edge case.
pub fn validate_oracle_observation(
    price: i128,
    price_timestamp: u64,
    expiry: u64,
    now: u64,
    max_staleness: u64,
) -> Result<(), Error> {
    if price <= 0 {
        return Err(Error::InvalidOraclePrice);
    }
    if price_timestamp < expiry {
        return Err(Error::OraclePricePredatesExpiry);
    }
    if price_timestamp > now {
        return Err(Error::OraclePriceFromFuture);
    }
    if now - price_timestamp > max_staleness {
        return Err(Error::OraclePriceStale);
    }
    Ok(())
}

/// Pro-rata share of a pool, used for both the holder payoff pool and the
/// writer residual pool.
///
/// Callers pass the *remaining* balance and *remaining* pool, decrementing both
/// after every claim. Because a claimant whose balance covers the whole
/// remaining balance receives the whole remaining pool, the final claimant
/// always sweeps the remainder — so integer division can never strand dust in
/// the vault.
pub fn pro_rata_share(balance: i128, remaining_balance: i128, remaining_pool: i128) -> i128 {
    if balance <= 0 || remaining_balance <= 0 || remaining_pool <= 0 {
        return 0;
    }
    if balance >= remaining_balance {
        return remaining_pool;
    }
    let share = balance
        .checked_mul(remaining_pool)
        .map(|v| v / remaining_balance)
        .unwrap_or(0);
    if share > remaining_pool {
        remaining_pool
    } else {
        share
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collateral_required_call_and_put() {
        // Calls lock one whole underlying unit per contract.
        assert_eq!(
            collateral_required(OptionKind::Call, PRICE_SCALE, 10).unwrap(),
            10 * CONTRACT_SIZE
        );
        // Puts lock the strike (max payoff) per contract.
        assert_eq!(
            collateral_required(OptionKind::Put, PRICE_SCALE, 10).unwrap(),
            10 * PRICE_SCALE
        );
    }

    #[test]
    fn collateral_required_rejects_junk() {
        assert_eq!(
            collateral_required(OptionKind::Put, 0, 10),
            Err(Error::InvalidParameter)
        );
        assert_eq!(
            collateral_required(OptionKind::Call, PRICE_SCALE, 0),
            Err(Error::InvalidParameter)
        );
        assert_eq!(
            collateral_required(OptionKind::Call, PRICE_SCALE, -1),
            Err(Error::InvalidParameter)
        );
    }

    #[test]
    fn moneyness_including_exactly_at_the_strike() {
        let strike = PRICE_SCALE;
        // Call is ITM above the strike, worthless at or below it.
        assert!(is_in_the_money(OptionKind::Call, strike, strike + 1));
        assert!(!is_in_the_money(OptionKind::Call, strike, strike));
        assert!(!is_in_the_money(OptionKind::Call, strike, strike - 1));
        // Put is ITM below the strike, worthless at or above it.
        assert!(is_in_the_money(OptionKind::Put, strike, strike - 1));
        assert!(!is_in_the_money(OptionKind::Put, strike, strike));
        assert!(!is_in_the_money(OptionKind::Put, strike, strike + 1));
    }

    #[test]
    fn put_payoff_is_vanilla_intrinsic_value() {
        // Put, strike $1.00, spot $0.50 -> $0.50 per contract of payoff.
        let strike = PRICE_SCALE;
        let price = PRICE_SCALE / 2;
        let supply = 10;
        let collateral = supply * strike;
        let (payout, residual) =
            settlement_pools(OptionKind::Put, strike, price, supply, collateral).unwrap();
        assert_eq!(payout, supply * (strike - price));
        assert_eq!(residual, supply * price);
        assert_eq!(payout + residual, collateral);
    }

    #[test]
    fn call_payoff_converts_intrinsic_to_underlying() {
        // Call, strike $1.00, spot $1.50: intrinsic = $0.50 * CONTRACT_SIZE
        // units of quote, converted to underlying at $1.50 gives exactly
        // one third of a whole underlying unit per contract.
        let strike = PRICE_SCALE;
        let price = PRICE_SCALE * 3 / 2;
        let supply = 10;
        let collateral = supply * CONTRACT_SIZE;
        let (payout, residual) =
            settlement_pools(OptionKind::Call, strike, price, supply, collateral).unwrap();
        // 10 * 5e6 * 1e7 / 1.5e7 == 33_333_333 (floored).
        assert_eq!(payout, 33_333_333);
        assert_eq!(residual, collateral - payout);
        assert!(payout + residual == collateral);
        // The payoff is always covered by the escrow.
        assert!(payout <= collateral);
    }

    #[test]
    fn otm_and_atm_pay_nothing_to_holders() {
        let strike = PRICE_SCALE;
        let supply = 10;
        let collateral = supply * CONTRACT_SIZE;
        // Exactly at the strike -> worthless.
        assert_eq!(
            settlement_pools(OptionKind::Call, strike, strike, supply, collateral).unwrap(),
            (0, collateral)
        );
        // Below the strike for a call -> worthless.
        assert_eq!(
            settlement_pools(OptionKind::Call, strike, strike / 2, supply, collateral).unwrap(),
            (0, collateral)
        );
        // Above the strike for a put -> worthless.
        assert_eq!(
            settlement_pools(
                OptionKind::Put,
                strike,
                strike * 2,
                supply,
                supply * strike
            )
            .unwrap(),
            (0, supply * strike)
        );
    }

    #[test]
    fn payoff_is_capped_at_escrow() {
        // A deeply ITM put cannot pay more than was locked.
        let strike = PRICE_SCALE;
        let supply = 10;
        let collateral = 5; // deliberately under-collateralized input
        let payout =
            payoff_pool(OptionKind::Put, strike, 1, supply, collateral).unwrap();
        assert_eq!(payout, collateral);
    }

    #[test]
    fn oracle_observation_policy() {
        let expiry = 1_000u64;
        let now = 1_100u64;
        let max_staleness = 120u64;
        // Fresh, post-expiry observation is accepted.
        assert!(validate_oracle_observation(1, expiry, expiry, now, max_staleness).is_ok());
        assert!(validate_oracle_observation(1, now, expiry, now, max_staleness).is_ok());
        // Pre-expiry observation is rejected (delayed feed).
        assert_eq!(
            validate_oracle_observation(1, expiry - 1, expiry, now, max_staleness),
            Err(Error::OraclePricePredatesExpiry)
        );
        // Post-expiry but older than the staleness window is rejected.
        assert_eq!(
            validate_oracle_observation(1, 1_300, expiry, 1_500, max_staleness),
            Err(Error::OraclePriceStale)
        );
        // Observation exactly `max_staleness` old is still accepted.
        assert!(validate_oracle_observation(1, 1_500 - max_staleness, expiry, 1_500, max_staleness)
            .is_ok());
        // Observation from the future is rejected.
        assert_eq!(
            validate_oracle_observation(1, now + 1, expiry, now, max_staleness),
            Err(Error::OraclePriceFromFuture)
        );
        // Non-positive prices are rejected.
        assert_eq!(
            validate_oracle_observation(0, now, expiry, now, max_staleness),
            Err(Error::InvalidOraclePrice)
        );
    }

    #[test]
    fn pro_rata_share_drains_the_pool_exactly() {
        // Three holders with uneven balances.
        let mut remaining_balance = 100i128;
        let mut remaining_pool = 1_000i128;
        let balances = [30i128, 30, 40];
        let mut paid = 0i128;
        for b in balances {
            let share = pro_rata_share(b, remaining_balance, remaining_pool);
            remaining_balance -= b;
            remaining_pool -= share;
            paid += share;
        }
        assert_eq!(paid, 1_000, "the whole pool must be distributed");
        assert_eq!(remaining_pool, 0);
        assert_eq!(remaining_balance, 0);
    }

    #[test]
    fn pro_rata_share_edge_cases() {
        assert_eq!(pro_rata_share(0, 10, 100), 0);
        assert_eq!(pro_rata_share(10, 0, 100), 0);
        assert_eq!(pro_rata_share(10, 10, 0), 0);
        // Last claimant sweeps the remainder.
        assert_eq!(pro_rata_share(10, 10, 7), 7);
    }
}
