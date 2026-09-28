//! Tick-based pricing: price bins, tick ↔ sqrt-price conversion, per-tick
//! liquidity/fee bookkeeping and the tick bitmap.
//!
//! A tick `i` marks the price `P(i) = 1.0001^i`; the contract stores
//! `√P(i)` in Q64.64. Liquidity is only ever added between two *initialized*
//! ticks that are multiples of the pool's `tick_spacing`, so the price axis is
//! partitioned into bins `[i·s, (i+1)·s)` inside which liquidity is constant.
//! See README §1–2 for the derivations of the bounds used below.

use soroban_sdk::contracttype;

use crate::math::{add_delta, div_256_by_128, full_mul, Q64};

/// Lowest supported tick. `√P(MIN_TICK) ≈ 2^-32`, i.e. `P ≈ 5.4e-20`.
pub const MIN_TICK: i32 = -443_636;
/// Highest supported tick. `√P(MAX_TICK) ≈ 2^32`, i.e. `P ≈ 1.8e19`.
pub const MAX_TICK: i32 = -MIN_TICK;
/// `sqrt_price_at_tick(MIN_TICK)` in Q64.64.
pub const MIN_SQRT_PRICE: u128 = sqrt_price_at_tick(MIN_TICK);
/// `sqrt_price_at_tick(MAX_TICK)` in Q64.64.
pub const MAX_SQRT_PRICE: u128 = sqrt_price_at_tick(MAX_TICK);
/// Upper bound on configurable tick spacing.
pub const MAX_TICK_SPACING: i32 = 16_384;
/// Bits per bitmap word.
pub const WORD_BITS: i32 = 128;

/// `2^128 / 1.0001^(2^(i-1))` in Q128.128 for bit `i` of `|tick|`, rounded up
/// (identical to the audited Uniswap v3 `TickMath` constants; re-derived and
/// checked at 120-digit precision). Bits 0..=18 cover `|tick| ≤ 443_636`.
const RATIOS: [u128; 19] = [
    0xfffcb933bd6fad37aa2d162d1a594001,
    0xfff97272373d413259a46990580e213a,
    0xfff2e50f5f656932ef12357cf3c7fdcc,
    0xffe5caca7e10e4e61c3624eaa0941cd0,
    0xffcb9843d60f6159c9db58835c926644,
    0xff973b41fa98c081472e6896dfb254c0,
    0xff2ea16466c96a3843ec78b326b52861,
    0xfe5dee046a99a2a811c461f1969c3053,
    0xfcbe86c7900a88aedcffc83b479aa3a4,
    0xf987a7253ac413176f2b074cf7815e54,
    0xf3392b0822b70005940c7a398e4b70f3,
    0xe7159475a2c29b7443b29c7fa6e889d9,
    0xd097f3bdfd2022b8845ad8f792aa5825,
    0xa9f746462d870fdf8a65dc1f90e061e5,
    0x70d869a156d2a1b890bb3df62baf32f7,
    0x31be135f97d08fd981231505542fcfa6,
    0x09aa508b5b7a84e1c677de54f3e99bc9,
    0x005d6af8dedb81196699c329225ee604,
    0x00002216e584f5fa1ea926041bedfe98,
];

/// `√(1.0001^tick)` in Q64.64. Strictly increasing in `tick`.
///
/// Computes `1/√(1.0001^|tick|)` in Q128.128 by multiplying the precomputed
/// factors for each set bit of `|tick|` (≤ 19 `full_mul`s, no division), then
/// inverts for positive ticks. Panics only for ticks outside
/// `[MIN_TICK, MAX_TICK]`, which callers validate first.
pub const fn sqrt_price_at_tick(tick: i32) -> u128 {
    let abs = tick.unsigned_abs();
    assert!(abs <= MAX_TICK as u32, "tick out of range");
    if abs == 0 {
        return Q64;
    }

    // `ratio` is a Q128.128 number < 1 once the first factor is applied; the
    // implicit starting value 1.0 (= 2^128) is not representable, so the first
    // set bit loads its factor directly.
    let mut ratio: u128 = 0;
    let mut i = 0;
    while i < RATIOS.len() {
        if abs & (1 << i) != 0 {
            ratio = if ratio == 0 {
                RATIOS[i]
            } else {
                full_mul(ratio, RATIOS[i]).0
            };
        }
        i += 1;
    }

    if tick > 0 {
        // 1/r in Q64.64 = 2^192 / ratio, rounded up. ratio > 2^64 so the
        // quotient fits (it is ≤ 2^96 for |tick| ≤ MAX_TICK).
        let (q, r) = div_256_by_128(1 << 64, 0, ratio);
        if r != 0 {
            q + 1
        } else {
            q
        }
    } else {
        // Q128.128 → Q64.64, rounded up.
        (ratio >> 64)
            + if ratio & (u64::MAX as u128) != 0 {
                1
            } else {
                0
            }
    }
}

/// Greatest tick `t ∈ [lo, hi]` with `sqrt_price_at_tick(t) <= sqrt_price`.
///
/// Binary search over the monotone map; callers pass the tightest known
/// bracket (inside a swap that is a single bitmap word, ≤ 7 iterations for
/// spacing 1).
pub fn tick_at_sqrt_price_in(sqrt_price: u128, mut lo: i32, mut hi: i32) -> i32 {
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        if sqrt_price_at_tick(mid) <= sqrt_price {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Greatest tick whose sqrt price is `<= sqrt_price`, over the full range.
///
/// The bracket is first narrowed with `log2`: `√P ∈ [2^k, 2^(k+1))` implies
/// `tick ∈ [2k·log_1.0001(2), 2(k+1)·log_1.0001(2))` with
/// `log_1.0001(2) ∈ (6931, 6932)`. Requires `sqrt_price >= MIN_SQRT_PRICE`.
pub fn tick_at_sqrt_price(sqrt_price: u128) -> i32 {
    // k = floor(log2(√P)), where the stored value is √P · 2^64.
    let k = 63 - sqrt_price.leading_zeros() as i32;
    let lo = (2 * k * 6931).min(2 * k * 6932);
    let hi = (2 * (k + 1) * 6931).max(2 * (k + 1) * 6932);
    tick_at_sqrt_price_in(
        sqrt_price,
        lo.clamp(MIN_TICK, MAX_TICK),
        hi.clamp(MIN_TICK, MAX_TICK),
    )
}

/// Validates a position's tick range against the pool's spacing.
pub fn check_ticks(lower: i32, upper: i32, spacing: i32) -> bool {
    lower < upper
        && lower >= MIN_TICK
        && upper <= MAX_TICK
        && lower % spacing == 0
        && upper % spacing == 0
}

/// Per-tick liquidity cap so that the sum over every usable tick fits in u128.
pub fn max_liquidity_per_tick(spacing: i32) -> u128 {
    let min = (MIN_TICK / spacing) * spacing;
    let max = (MAX_TICK / spacing) * spacing;
    let num_ticks = ((max - min) / spacing) as u128 + 1;
    u128::MAX / num_ticks
}

/// State of an initialized tick.
#[contracttype]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TickInfo {
    /// Total liquidity referencing this tick (as lower or upper bound).
    pub liquidity_gross: u128,
    /// Liquidity added when the price crosses this tick left → right.
    pub liquidity_net: i128,
    /// Fee growth per unit of liquidity on the *other* side of this tick
    /// (relative to the current price), Q64.64, modular.
    pub fee_growth_outside_0: u128,
    pub fee_growth_outside_1: u128,
}

impl TickInfo {
    /// Applies a liquidity delta to this tick. Returns whether the tick
    /// flipped between initialized and uninitialized, or `None` on overflow
    /// or when the per-tick cap is exceeded.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        tick: i32,
        tick_current: i32,
        delta: i128,
        fee_growth_global_0: u128,
        fee_growth_global_1: u128,
        upper: bool,
        max_liquidity: u128,
    ) -> Option<bool> {
        let gross_before = self.liquidity_gross;
        let gross_after = add_delta(gross_before, delta)?;
        if gross_after > max_liquidity {
            return None;
        }
        if gross_before == 0 && tick <= tick_current {
            // Convention: all growth so far happened below the tick.
            self.fee_growth_outside_0 = fee_growth_global_0;
            self.fee_growth_outside_1 = fee_growth_global_1;
        }
        self.liquidity_gross = gross_after;
        self.liquidity_net = if upper {
            self.liquidity_net.checked_sub(delta)?
        } else {
            self.liquidity_net.checked_add(delta)?
        };
        Some((gross_after == 0) != (gross_before == 0))
    }

    /// Transitions the tick as the price crosses it; returns `liquidity_net`.
    ///
    /// Fee growth accumulators are modular (Q64.64 mod 2^128): only their
    /// differences are ever consumed, so wrapping subtraction is exact.
    pub fn cross(&mut self, fee_growth_global_0: u128, fee_growth_global_1: u128) -> i128 {
        self.fee_growth_outside_0 = fee_growth_global_0.wrapping_sub(self.fee_growth_outside_0);
        self.fee_growth_outside_1 = fee_growth_global_1.wrapping_sub(self.fee_growth_outside_1);
        self.liquidity_net
    }
}

/// Fee growth per unit of liquidity accrued strictly inside `[lower, upper)`.
///
/// Only swaps executed while the price was inside the range contribute, which
/// is what restricts fee rewards to LPs whose ranges are actively crossed.
pub fn fee_growth_inside(
    lower: &TickInfo,
    upper: &TickInfo,
    tick_lower: i32,
    tick_upper: i32,
    tick_current: i32,
    global_0: u128,
    global_1: u128,
) -> (u128, u128) {
    let (below_0, below_1) = if tick_current >= tick_lower {
        (lower.fee_growth_outside_0, lower.fee_growth_outside_1)
    } else {
        (
            global_0.wrapping_sub(lower.fee_growth_outside_0),
            global_1.wrapping_sub(lower.fee_growth_outside_1),
        )
    };
    let (above_0, above_1) = if tick_current < tick_upper {
        (upper.fee_growth_outside_0, upper.fee_growth_outside_1)
    } else {
        (
            global_0.wrapping_sub(upper.fee_growth_outside_0),
            global_1.wrapping_sub(upper.fee_growth_outside_1),
        )
    };
    (
        global_0.wrapping_sub(below_0).wrapping_sub(above_0),
        global_1.wrapping_sub(below_1).wrapping_sub(above_1),
    )
}

/// Maps a (spacing-compressed) tick to its bitmap word index and bit.
#[inline]
pub fn bitmap_position(compressed: i32) -> (i32, u32) {
    (compressed >> 7, (compressed & (WORD_BITS - 1)) as u32)
}

/// Floor division of `tick` by `spacing` (rounds toward −∞).
#[inline]
pub fn compress(tick: i32, spacing: i32) -> i32 {
    tick.div_euclid(spacing)
}

/// Searches one bitmap word for the next initialized tick.
///
/// * `lte = true`: the nearest set bit at or below `bit` (price moving down).
/// * `lte = false`: the nearest set bit strictly above `bit` is looked up by
///   the caller passing `compressed + 1`, then this returns the nearest set bit
///   at or above `bit`.
///
/// Returns `(bit_index, initialized)`; if no bit is set, the word boundary in
/// the search direction is returned with `initialized = false`, which bounds
/// every swap step to at most one word of ticks.
#[inline]
pub fn next_bit_in_word(word: u128, bit: u32, lte: bool) -> (u32, bool) {
    if lte {
        let mask = if bit == 127 {
            u128::MAX
        } else {
            (1u128 << (bit + 1)) - 1
        };
        let masked = word & mask;
        if masked != 0 {
            (127 - masked.leading_zeros(), true)
        } else {
            (0, false)
        }
    } else {
        let masked = word & !((1u128 << bit) - 1);
        if masked != 0 {
            (masked.trailing_zeros(), true)
        } else {
            (127, false)
        }
    }
}
