//! Concentrated liquidity AMM for Soroban.
//!
//! Liquidity providers deposit into custom price ranges `[tick_lower,
//! tick_upper)`; swaps walk the tick bitmap and execute segment by segment,
//! crossing as many initialized ticks as needed to fill the order. Fees are
//! accumulated per unit of *active* liquidity, so only positions whose range
//! contains the price while a swap executes earn them.
//!
//! Module layout:
//! * [`math`]  – 256-bit-intermediate fixed-point math and the swap step.
//! * [`ticks`] – tick ↔ price conversion, tick state, bitmap search.
//!
//! The derivations behind every formula live in `README.md`.

#![no_std]

pub mod math;
pub mod ticks;

#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

use math::{
    add_delta, amount0_delta, amount1_delta, compute_swap_step, mul_div, FEE_DENOMINATOR, Q64,
};
use ticks::{
    bitmap_position, check_ticks, compress, fee_growth_inside, max_liquidity_per_tick,
    next_bit_in_word, sqrt_price_at_tick, tick_at_sqrt_price, tick_at_sqrt_price_in, TickInfo,
    MAX_SQRT_PRICE, MAX_TICK, MAX_TICK_SPACING, MIN_SQRT_PRICE, MIN_TICK, WORD_BITS,
};

/// Highest fee tier accepted at initialization (10%).
pub const MAX_FEE_PIPS: u32 = 100_000;

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const PERSISTENT_BUMP: u32 = 120 * DAY_IN_LEDGERS;
const PERSISTENT_THRESHOLD: u32 = PERSISTENT_BUMP - 7 * DAY_IN_LEDGERS;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    IdenticalTokens = 3,
    InvalidFee = 4,
    InvalidTickSpacing = 5,
    InvalidSqrtPrice = 6,
    InvalidTickRange = 7,
    InvalidAmount = 8,
    InvalidPriceLimit = 9,
    SlippageExceeded = 10,
    TickLiquidityOverflow = 11,
    InsufficientPositionLiquidity = 12,
    PositionNotFound = 13,
    ArithmeticOverflow = 14,
}

/// Immutable pool parameters.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub token0: Address,
    pub token1: Address,
    /// Swap fee in hundredths of a basis point (3000 = 0.30%).
    pub fee_pips: u32,
    pub tick_spacing: i32,
    pub max_liquidity_per_tick: u128,
}

/// Mutable global pool state.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolState {
    /// √(token1/token0) in Q64.64.
    pub sqrt_price: u128,
    /// Greatest tick with `sqrt_price_at_tick(tick) <= sqrt_price`.
    pub tick: i32,
    /// Liquidity of all positions whose range contains the current price.
    pub liquidity: u128,
    /// Cumulative fees per unit of active liquidity, Q64.64 (modular).
    pub fee_growth_global_0: u128,
    pub fee_growth_global_1: u128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PositionKey {
    pub owner: Address,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

#[contracttype]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Position {
    pub liquidity: u128,
    pub fee_growth_inside_0_last: u128,
    pub fee_growth_inside_1_last: u128,
    /// Principal from burns plus accrued fees, awaiting `collect`.
    pub tokens_owed_0: u128,
    pub tokens_owed_1: u128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapResult {
    /// Input actually charged (including fees).
    pub amount_in: i128,
    pub amount_out: i128,
    pub sqrt_price: u128,
    pub tick: i32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
enum DataKey {
    Config,
    Pool,
    Tick(i32),
    Word(i32),
    Position(PositionKey),
}

#[contract]
pub struct ClAmm;

#[contractimpl]
impl ClAmm {
    /// Creates the pool at an initial price. `fee_pips` < [`MAX_FEE_PIPS`],
    /// `tick_spacing ∈ [1, 16384]`, `sqrt_price ∈ [MIN_SQRT_PRICE, MAX_SQRT_PRICE)`.
    pub fn initialize(
        env: Env,
        token0: Address,
        token1: Address,
        fee_pips: u32,
        tick_spacing: i32,
        sqrt_price: u128,
    ) -> Result<(), Error> {
        let store = env.storage().instance();
        if store.has(&DataKey::Config) {
            return Err(Error::AlreadyInitialized);
        }
        if token0 == token1 {
            return Err(Error::IdenticalTokens);
        }
        if fee_pips > MAX_FEE_PIPS {
            return Err(Error::InvalidFee);
        }
        if !(1..=MAX_TICK_SPACING).contains(&tick_spacing) {
            return Err(Error::InvalidTickSpacing);
        }
        if !(MIN_SQRT_PRICE..MAX_SQRT_PRICE).contains(&sqrt_price) {
            return Err(Error::InvalidSqrtPrice);
        }

        let config = Config {
            token0,
            token1,
            fee_pips,
            tick_spacing,
            max_liquidity_per_tick: max_liquidity_per_tick(tick_spacing),
        };
        let pool = PoolState {
            sqrt_price,
            tick: tick_at_sqrt_price(sqrt_price),
            liquidity: 0,
            fee_growth_global_0: 0,
            fee_growth_global_1: 0,
        };
        store.set(&DataKey::Config, &config);
        store.set(&DataKey::Pool, &pool);
        bump_instance(&env);
        env.events()
            .publish((symbol_short!("init"),), (sqrt_price, pool.tick));
        Ok(())
    }

    /// Adds `liquidity` to `owner`'s position in `[tick_lower, tick_upper)`,
    /// pulling the required token amounts (rounded up) from `owner`.
    pub fn mint(
        env: Env,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity: u128,
        amount0_max: i128,
        amount1_max: i128,
    ) -> Result<(i128, i128), Error> {
        owner.require_auth();
        if liquidity == 0 || liquidity > i128::MAX as u128 {
            return Err(Error::InvalidAmount);
        }
        let config = load_config(&env)?;
        let (a0, a1) = modify_position(
            &env,
            &config,
            &owner,
            tick_lower,
            tick_upper,
            liquidity as i128,
        )?;
        let (a0, a1) = (to_i128(a0)?, to_i128(a1)?);
        if a0 > amount0_max || a1 > amount1_max {
            return Err(Error::SlippageExceeded);
        }

        let this = env.current_contract_address();
        if a0 > 0 {
            token::Client::new(&env, &config.token0).transfer(&owner, &this, &a0);
        }
        if a1 > 0 {
            token::Client::new(&env, &config.token1).transfer(&owner, &this, &a1);
        }
        env.events().publish(
            (symbol_short!("mint"), owner),
            (tick_lower, tick_upper, liquidity, a0, a1),
        );
        Ok((a0, a1))
    }

    /// Removes `liquidity` from a position. The principal (rounded down) is
    /// credited to the position's owed balances together with accrued fees;
    /// call [`ClAmm::collect`] to withdraw. `liquidity = 0` just checkpoints fees.
    pub fn burn(
        env: Env,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity: u128,
    ) -> Result<(i128, i128), Error> {
        owner.require_auth();
        if liquidity > i128::MAX as u128 {
            return Err(Error::InvalidAmount);
        }
        let config = load_config(&env)?;
        let (a0, a1) = modify_position(
            &env,
            &config,
            &owner,
            tick_lower,
            tick_upper,
            -(liquidity as i128),
        )?;

        let (a0, a1) = (to_i128(a0)?, to_i128(a1)?);
        env.events().publish(
            (symbol_short!("burn"), owner),
            (tick_lower, tick_upper, liquidity, a0, a1),
        );
        Ok((a0, a1))
    }

    /// Transfers up to `amount0_max` / `amount1_max` of the position's owed
    /// tokens to `owner`. Returns the amounts paid.
    pub fn collect(
        env: Env,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        amount0_max: i128,
        amount1_max: i128,
    ) -> Result<(i128, i128), Error> {
        owner.require_auth();
        if amount0_max < 0 || amount1_max < 0 {
            return Err(Error::InvalidAmount);
        }
        let config = load_config(&env)?;
        let key = DataKey::Position(PositionKey {
            owner: owner.clone(),
            tick_lower,
            tick_upper,
        });
        let Some(mut pos) = env.storage().persistent().get::<_, Position>(&key) else {
            // Nothing is owed on an empty/closed position.
            return Ok((0, 0));
        };

        let a0 = pos.tokens_owed_0.min(amount0_max as u128);
        let a1 = pos.tokens_owed_1.min(amount1_max as u128);
        pos.tokens_owed_0 -= a0;
        pos.tokens_owed_1 -= a1;
        if pos == Position::default() {
            env.storage().persistent().remove(&key);
        } else {
            save_persistent(&env, &key, &pos);
        }

        let (a0, a1) = (a0 as i128, a1 as i128);
        let this = env.current_contract_address();
        if a0 > 0 {
            token::Client::new(&env, &config.token0).transfer(&this, &owner, &a0);
        }
        if a1 > 0 {
            token::Client::new(&env, &config.token1).transfer(&this, &owner, &a1);
        }
        env.events().publish(
            (symbol_short!("collect"), owner),
            (tick_lower, tick_upper, a0, a1),
        );
        Ok((a0, a1))
    }

    /// Exact-input swap. `zero_for_one` sells token0 for token1 (price moves
    /// down). The order walks across as many ticks as needed until
    /// `amount_in` is exhausted or `sqrt_price_limit` is reached
    /// (`0` = no limit); only the input actually consumed is charged.
    pub fn swap(
        env: Env,
        sender: Address,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit: u128,
        min_amount_out: i128,
    ) -> Result<SwapResult, Error> {
        sender.require_auth();
        if amount_in <= 0 {
            return Err(Error::InvalidAmount);
        }
        let config = load_config(&env)?;
        let mut pool = load_pool(&env)?;

        let limit = match (sqrt_price_limit, zero_for_one) {
            (0, true) => MIN_SQRT_PRICE + 1,
            (0, false) => MAX_SQRT_PRICE - 1,
            (l, _) => l,
        };
        let limit_ok = if zero_for_one {
            limit < pool.sqrt_price && limit > MIN_SQRT_PRICE
        } else {
            limit > pool.sqrt_price && limit < MAX_SQRT_PRICE
        };
        if !limit_ok {
            return Err(Error::InvalidPriceLimit);
        }

        let mut remaining = amount_in as u128;
        let mut amount_out: u128 = 0;
        let mut word_cache: Option<(i32, u128)> = None;

        while remaining != 0 && pool.sqrt_price != limit {
            let sqrt_start = pool.sqrt_price;
            let (tick_next, initialized) = next_initialized_tick(
                &env,
                &mut word_cache,
                pool.tick,
                config.tick_spacing,
                zero_for_one,
            );
            let tick_next = tick_next.clamp(MIN_TICK, MAX_TICK);
            let sqrt_next = sqrt_price_at_tick(tick_next);
            let target = if zero_for_one {
                sqrt_next.max(limit)
            } else {
                sqrt_next.min(limit)
            };

            let step = compute_swap_step(
                sqrt_start,
                target,
                pool.liquidity,
                remaining,
                config.fee_pips,
            )
            .ok_or(Error::ArithmeticOverflow)?;
            pool.sqrt_price = step.sqrt_price_next;
            // compute_swap_step guarantees amount_in + fee <= remaining.
            remaining -= step.amount_in + step.fee_amount;
            amount_out = amount_out
                .checked_add(step.amount_out)
                .ok_or(Error::ArithmeticOverflow)?;

            if pool.liquidity > 0 && step.fee_amount > 0 {
                let growth = mul_div(step.fee_amount, Q64, pool.liquidity)
                    .ok_or(Error::ArithmeticOverflow)?;
                if zero_for_one {
                    pool.fee_growth_global_0 = pool.fee_growth_global_0.wrapping_add(growth);
                } else {
                    pool.fee_growth_global_1 = pool.fee_growth_global_1.wrapping_add(growth);
                }
            }

            if pool.sqrt_price == sqrt_next {
                // Reached the bin boundary: cross it and activate/deactivate
                // the liquidity of every position that starts/ends here.
                if initialized {
                    let key = DataKey::Tick(tick_next);
                    let mut info: TickInfo =
                        env.storage().persistent().get(&key).unwrap_or_default();
                    let net = info.cross(pool.fee_growth_global_0, pool.fee_growth_global_1);
                    save_persistent(&env, &key, &info);
                    let net = if zero_for_one {
                        net.checked_neg().ok_or(Error::ArithmeticOverflow)?
                    } else {
                        net
                    };
                    pool.liquidity =
                        add_delta(pool.liquidity, net).ok_or(Error::ArithmeticOverflow)?;
                }
                pool.tick = if zero_for_one {
                    tick_next - 1
                } else {
                    tick_next
                };
            } else if pool.sqrt_price != sqrt_start {
                // Stopped inside the bin: the new tick lies between the old
                // tick and the bin boundary, so search only that bracket.
                pool.tick = if zero_for_one {
                    tick_at_sqrt_price_in(pool.sqrt_price, tick_next, pool.tick)
                } else {
                    tick_at_sqrt_price_in(pool.sqrt_price, pool.tick, tick_next - 1)
                };
            }
        }

        let amount_in_used = to_i128(amount_in as u128 - remaining)?;
        let amount_out = to_i128(amount_out)?;
        if amount_out < min_amount_out {
            return Err(Error::SlippageExceeded);
        }
        env.storage().instance().set(&DataKey::Pool, &pool);
        bump_instance(&env);

        let (token_in, token_out) = if zero_for_one {
            (&config.token0, &config.token1)
        } else {
            (&config.token1, &config.token0)
        };
        let this = env.current_contract_address();
        token::Client::new(&env, token_in).transfer(&sender, &this, &amount_in_used);
        if amount_out > 0 {
            token::Client::new(&env, token_out).transfer(&this, &sender, &amount_out);
        }

        env.events().publish(
            (symbol_short!("swap"), sender),
            (
                zero_for_one,
                amount_in_used,
                amount_out,
                pool.sqrt_price,
                pool.tick,
            ),
        );
        Ok(SwapResult {
            amount_in: amount_in_used,
            amount_out,
            sqrt_price: pool.sqrt_price,
            tick: pool.tick,
        })
    }

    // ---------------------------------------------------------------- views

    pub fn config(env: Env) -> Result<Config, Error> {
        load_config(&env)
    }

    pub fn pool(env: Env) -> Result<PoolState, Error> {
        load_pool(&env)
    }

    pub fn tick(env: Env, tick: i32) -> TickInfo {
        env.storage()
            .persistent()
            .get(&DataKey::Tick(tick))
            .unwrap_or_default()
    }

    pub fn position(env: Env, owner: Address, tick_lower: i32, tick_upper: i32) -> Position {
        env.storage()
            .persistent()
            .get(&DataKey::Position(PositionKey {
                owner,
                tick_lower,
                tick_upper,
            }))
            .unwrap_or_default()
    }

    /// Largest liquidity mintable in `[tick_lower, tick_upper)` at the current
    /// price with at most `amount0` / `amount1`.
    pub fn liquidity_for_amounts(
        env: Env,
        tick_lower: i32,
        tick_upper: i32,
        amount0: i128,
        amount1: i128,
    ) -> Result<u128, Error> {
        let config = load_config(&env)?;
        if !check_ticks(tick_lower, tick_upper, config.tick_spacing) {
            return Err(Error::InvalidTickRange);
        }
        if amount0 < 0 || amount1 < 0 {
            return Err(Error::InvalidAmount);
        }
        let pool = load_pool(&env)?;
        math::liquidity_for_amounts(
            pool.sqrt_price,
            sqrt_price_at_tick(tick_lower),
            sqrt_price_at_tick(tick_upper),
            amount0 as u128,
            amount1 as u128,
        )
        .ok_or(Error::ArithmeticOverflow)
    }
}

// ------------------------------------------------------------------ internals

/// Applies a signed liquidity delta to a position and its bounding ticks and
/// returns the token amounts owed to (`delta > 0`, rounded up) or by
/// (`delta < 0`, rounded down) the pool.
fn modify_position(
    env: &Env,
    config: &Config,
    owner: &Address,
    tick_lower: i32,
    tick_upper: i32,
    delta: i128,
) -> Result<(u128, u128), Error> {
    if !check_ticks(tick_lower, tick_upper, config.tick_spacing) {
        return Err(Error::InvalidTickRange);
    }
    let mut pool = load_pool(env)?;
    let storage = env.storage().persistent();

    let lower_key = DataKey::Tick(tick_lower);
    let upper_key = DataKey::Tick(tick_upper);
    let mut lower: TickInfo = storage.get(&lower_key).unwrap_or_default();
    let mut upper: TickInfo = storage.get(&upper_key).unwrap_or_default();

    let pos_key = DataKey::Position(PositionKey {
        owner: owner.clone(),
        tick_lower,
        tick_upper,
    });
    let mut pos: Position = storage.get(&pos_key).unwrap_or_default();
    if delta == 0 && pos.liquidity == 0 {
        return Err(Error::PositionNotFound);
    }
    if delta < 0 && pos.liquidity < delta.unsigned_abs() {
        return Err(Error::InsufficientPositionLiquidity);
    }

    let (mut flipped_lower, mut flipped_upper) = (false, false);
    if delta != 0 {
        let (g0, g1) = (pool.fee_growth_global_0, pool.fee_growth_global_1);
        let max = config.max_liquidity_per_tick;
        flipped_lower = lower
            .update(tick_lower, pool.tick, delta, g0, g1, false, max)
            .ok_or(Error::TickLiquidityOverflow)?;
        flipped_upper = upper
            .update(tick_upper, pool.tick, delta, g0, g1, true, max)
            .ok_or(Error::TickLiquidityOverflow)?;
        if flipped_lower {
            flip_tick(env, tick_lower, config.tick_spacing);
        }
        if flipped_upper {
            flip_tick(env, tick_upper, config.tick_spacing);
        }
    }

    // Settle fees earned since the last checkpoint at the old liquidity.
    let (inside0, inside1) = fee_growth_inside(
        &lower,
        &upper,
        tick_lower,
        tick_upper,
        pool.tick,
        pool.fee_growth_global_0,
        pool.fee_growth_global_1,
    );
    let owed0 = mul_div(
        inside0.wrapping_sub(pos.fee_growth_inside_0_last),
        pos.liquidity,
        Q64,
    )
    .ok_or(Error::ArithmeticOverflow)?;
    let owed1 = mul_div(
        inside1.wrapping_sub(pos.fee_growth_inside_1_last),
        pos.liquidity,
        Q64,
    )
    .ok_or(Error::ArithmeticOverflow)?;
    pos.tokens_owed_0 = pos
        .tokens_owed_0
        .checked_add(owed0)
        .ok_or(Error::ArithmeticOverflow)?;
    pos.tokens_owed_1 = pos
        .tokens_owed_1
        .checked_add(owed1)
        .ok_or(Error::ArithmeticOverflow)?;
    pos.fee_growth_inside_0_last = inside0;
    pos.fee_growth_inside_1_last = inside1;
    pos.liquidity = add_delta(pos.liquidity, delta).ok_or(Error::ArithmeticOverflow)?;

    // Token amounts for the liquidity change, depending on where the current
    // price sits relative to the range (README §3).
    let abs = delta.unsigned_abs();
    let round_up = delta > 0;
    let sqrt_lower = sqrt_price_at_tick(tick_lower);
    let sqrt_upper = sqrt_price_at_tick(tick_upper);
    let (a0, a1) = if pool.tick < tick_lower {
        (
            amount0_delta(sqrt_lower, sqrt_upper, abs, round_up),
            Some(0),
        )
    } else if pool.tick < tick_upper {
        pool.liquidity = add_delta(pool.liquidity, delta).ok_or(Error::ArithmeticOverflow)?;
        env.storage().instance().set(&DataKey::Pool, &pool);
        (
            amount0_delta(pool.sqrt_price, sqrt_upper, abs, round_up),
            amount1_delta(sqrt_lower, pool.sqrt_price, abs, round_up),
        )
    } else {
        (
            Some(0),
            amount1_delta(sqrt_lower, sqrt_upper, abs, round_up),
        )
    };
    let a0 = a0.ok_or(Error::ArithmeticOverflow)?;
    let a1 = a1.ok_or(Error::ArithmeticOverflow)?;

    if delta < 0 {
        // Burned principal stays in the pool until collected.
        pos.tokens_owed_0 = pos
            .tokens_owed_0
            .checked_add(a0)
            .ok_or(Error::ArithmeticOverflow)?;
        pos.tokens_owed_1 = pos
            .tokens_owed_1
            .checked_add(a1)
            .ok_or(Error::ArithmeticOverflow)?;
    }

    if pos == Position::default() {
        storage.remove(&pos_key);
    } else {
        save_persistent(env, &pos_key, &pos);
    }
    for (key, info, flipped) in [
        (&lower_key, &lower, flipped_lower),
        (&upper_key, &upper, flipped_upper),
    ] {
        if flipped && delta < 0 {
            storage.remove(key);
        } else if delta != 0 {
            save_persistent(env, key, info);
        }
    }
    bump_instance(env);
    Ok((a0, a1))
}

/// Next initialized tick (or the word boundary) in the swap direction.
fn next_initialized_tick(
    env: &Env,
    cache: &mut Option<(i32, u128)>,
    tick: i32,
    spacing: i32,
    lte: bool,
) -> (i32, bool) {
    let compressed = compress(tick, spacing);
    let start = if lte { compressed } else { compressed + 1 };
    let (word_pos, bit) = bitmap_position(start);
    let word = match *cache {
        Some((pos, w)) if pos == word_pos => w,
        _ => {
            let w: u128 = env
                .storage()
                .persistent()
                .get(&DataKey::Word(word_pos))
                .unwrap_or(0);
            *cache = Some((word_pos, w));
            w
        }
    };
    let (found, initialized) = next_bit_in_word(word, bit, lte);
    let next_compressed = word_pos * WORD_BITS + found as i32;
    (next_compressed.saturating_mul(spacing), initialized)
}

fn flip_tick(env: &Env, tick: i32, spacing: i32) {
    let (word_pos, bit) = bitmap_position(tick / spacing);
    let key = DataKey::Word(word_pos);
    let word: u128 = env.storage().persistent().get(&key).unwrap_or(0);
    let word = word ^ (1u128 << bit);
    if word == 0 {
        env.storage().persistent().remove(&key);
    } else {
        save_persistent(env, &key, &word);
    }
}

fn load_config(env: &Env) -> Result<Config, Error> {
    env.storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(Error::NotInitialized)
}

fn load_pool(env: &Env) -> Result<PoolState, Error> {
    env.storage()
        .instance()
        .get(&DataKey::Pool)
        .ok_or(Error::NotInitialized)
}

fn save_persistent<V: soroban_sdk::IntoVal<Env, soroban_sdk::Val>>(
    env: &Env,
    key: &DataKey,
    value: &V,
) {
    let storage = env.storage().persistent();
    storage.set(key, value);
    storage.extend_ttl(key, PERSISTENT_THRESHOLD, PERSISTENT_BUMP);
}

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

fn to_i128(x: u128) -> Result<i128, Error> {
    i128::try_from(x).map_err(|_| Error::ArithmeticOverflow)
}

// Compile-time sanity checks on the fixed-point domain (README §1).
const _: () = {
    assert!(MIN_SQRT_PRICE > 0);
    assert!(MAX_SQRT_PRICE < u128::MAX >> 30);
    assert!((FEE_DENOMINATOR as u32) > MAX_FEE_PIPS);
};
