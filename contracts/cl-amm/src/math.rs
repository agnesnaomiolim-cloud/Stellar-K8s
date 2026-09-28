//! Fixed-point integer math for the concentrated liquidity AMM.
//!
//! All prices are square roots of `token1 / token0` encoded as **Q64.64**
//! unsigned fixed point (`u128`, 64 integer bits, 64 fractional bits).
//! Intermediate products are carried in 256 bits via [`full_mul`] and reduced
//! with a two-limb long division ([`div_256_by_128`]), so no step silently
//! truncates. Every function that can overflow returns `None` instead of
//! wrapping; rounding direction is always explicit and always favours the pool.
//!
//! No floating point, no host `U256` objects: everything is plain `u128`
//! arithmetic that compiles to a handful of WASM instructions per call, which
//! keeps multi-tick swaps well inside Soroban's CPU budget.

/// `1.0` in Q64.64.
pub const Q64: u128 = 1 << 64;
/// Fee denominator: fees are expressed in hundredths of a basis point.
pub const FEE_DENOMINATOR: u128 = 1_000_000;

const LO_MASK: u128 = u64::MAX as u128;

/// Full 128×128 → 256-bit product, returned as `(hi, lo)`.
#[inline]
pub const fn full_mul(a: u128, b: u128) -> (u128, u128) {
    let (a1, a0) = (a >> 64, a & LO_MASK);
    let (b1, b0) = (b >> 64, b & LO_MASK);

    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;

    // Middle column: at most 3 * (2^64 - 1) — never overflows u128.
    let mid = (p00 >> 64) + (p01 & LO_MASK) + (p10 & LO_MASK);
    let lo = (p00 & LO_MASK) | (mid << 64);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (hi, lo)
}

/// Divides the 256-bit value `hi:lo` by `d`, returning `(quotient, remainder)`.
///
/// Precondition: `hi < d` (equivalently the quotient fits in 128 bits).
/// Knuth algorithm D specialised to two 64-bit "digits" (Hacker's Delight
/// `divlu`), using native `u128` operations for every digit step.
#[inline]
pub const fn div_256_by_128(hi: u128, lo: u128, d: u128) -> (u128, u128) {
    const B: u128 = 1 << 64;
    // Normalise so the divisor's top bit is set.
    let s = d.leading_zeros();
    let v = d << s;
    let vn1 = v >> 64;
    let vn0 = v & LO_MASK;

    let un32 = if s == 0 {
        hi
    } else {
        (hi << s) | (lo >> (128 - s))
    };
    let un10 = lo << s;
    let un1 = un10 >> 64;
    let un0 = un10 & LO_MASK;

    // First quotient digit.
    let mut q1 = un32 / vn1;
    let mut rhat = un32 - q1 * vn1;
    while q1 >= B || q1 * vn0 > (rhat << 64) + un1 {
        q1 -= 1;
        rhat += vn1;
        if rhat >= B {
            break;
        }
    }
    let un21 = (un32 << 64)
        .wrapping_add(un1)
        .wrapping_sub(q1.wrapping_mul(v));

    // Second quotient digit.
    let mut q0 = un21 / vn1;
    let mut rhat = un21 - q0 * vn1;
    while q0 >= B || q0 * vn0 > (rhat << 64) + un0 {
        q0 -= 1;
        rhat += vn1;
        if rhat >= B {
            break;
        }
    }
    let rem = (un21 << 64)
        .wrapping_add(un0)
        .wrapping_sub(q0.wrapping_mul(v))
        >> s;
    ((q1 << 64) | q0, rem)
}

/// `floor(a * b / d)` together with the remainder, or `None` if `d == 0` or
/// the quotient does not fit in 128 bits.
#[inline]
pub const fn mul_div_rem(a: u128, b: u128, d: u128) -> Option<(u128, u128)> {
    if d == 0 {
        return None;
    }
    let (hi, lo) = full_mul(a, b);
    if hi == 0 {
        return Some((lo / d, lo % d));
    }
    if hi >= d {
        return None;
    }
    Some(div_256_by_128(hi, lo, d))
}

/// `floor(a * b / d)` with a 256-bit intermediate.
#[inline]
pub const fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    match mul_div_rem(a, b, d) {
        Some((q, _)) => Some(q),
        None => None,
    }
}

/// `ceil(a * b / d)` with a 256-bit intermediate.
#[inline]
pub fn mul_div_up(a: u128, b: u128, d: u128) -> Option<u128> {
    let (q, r) = mul_div_rem(a, b, d)?;
    if r == 0 {
        Some(q)
    } else {
        q.checked_add(1)
    }
}

/// Applies a signed liquidity delta to an unsigned liquidity value.
#[inline]
pub fn add_delta(x: u128, delta: i128) -> Option<u128> {
    if delta < 0 {
        x.checked_sub(delta.unsigned_abs())
    } else {
        x.checked_add(delta as u128)
    }
}

#[inline]
fn sort(a: u128, b: u128) -> (u128, u128) {
    if a > b {
        (b, a)
    } else {
        (a, b)
    }
}

/// Amount of token0 held by `liquidity` between two sqrt prices:
///
/// `Δx = L · (√Pb − √Pa) / (√Pa · √Pb)`  (all in real numbers; `·Q64` in fixed point)
///
/// Computed **exactly** (floor or ceil) without a 320-bit intermediate by
/// splitting the first division into quotient and remainder — see README §3.
pub fn amount0_delta(sqrt_a: u128, sqrt_b: u128, liquidity: u128, round_up: bool) -> Option<u128> {
    let (sa, sb) = sort(sqrt_a, sqrt_b);
    if sa == 0 {
        return None;
    }
    let diff = sb - sa;
    // t·sb + r = L·diff        (t < L because diff < sb, so it fits)
    let (t, r) = mul_div_rem(liquidity, diff, sb)?;
    // term1·sa + r1 = t·Q64
    let (term1, r1) = mul_div_rem(t, Q64, sa)?;
    // u·sb + r2 = r·Q64        (u < Q64 because r < sb)
    let (u, r2) = mul_div_rem(r, Q64, sb)?;
    // Δx = term1 + (r1 + u + r2/sb) / sa, with r1 < sa ≤ 2^96 and u < 2^64.
    let tail = r1.checked_add(u)?;
    let floor = term1.checked_add(tail / sa)?;
    if round_up && (tail % sa != 0 || r2 != 0) {
        floor.checked_add(1)
    } else {
        Some(floor)
    }
}

/// Amount of token1 held by `liquidity` between two sqrt prices:
///
/// `Δy = L · (√Pb − √Pa)`
pub fn amount1_delta(sqrt_a: u128, sqrt_b: u128, liquidity: u128, round_up: bool) -> Option<u128> {
    let (sa, sb) = sort(sqrt_a, sqrt_b);
    if round_up {
        mul_div_up(liquidity, sb - sa, Q64)
    } else {
        mul_div(liquidity, sb - sa, Q64)
    }
}

/// Next sqrt price after adding `amount` of token0 (price moves down).
/// Rounded **up** so the pool never gives away more token1 than it received.
///
/// `√P' = L / (L/√P + Δx)`
fn next_sqrt_price_from_amount0_in(sqrt_p: u128, liquidity: u128, amount: u128) -> Option<u128> {
    if amount == 0 {
        return Some(sqrt_p);
    }
    // Primary path: denominator expressed in token0 units, floored ⇒ result
    // rounded up, error < 1 unit of token0.
    if let Some(x) = mul_div(liquidity, Q64, sqrt_p) {
        if let Some(den) = x.checked_add(amount) {
            return mul_div_up(liquidity, Q64, den);
        }
    }
    // Fallback for extreme liquidity (L·Q64/√P ≥ 2^128): the equivalent form
    // L·√P / (L + Δx·√P) with the denominator floored (still rounds up).
    let den = liquidity.saturating_add(mul_div(amount, sqrt_p, Q64).unwrap_or(u128::MAX));
    mul_div_up(liquidity, sqrt_p, den)
}

/// Next sqrt price after adding `amount` of token1 (price moves up).
/// Rounded **down** so the pool never gives away more token0 than it received.
///
/// `√P' = √P + Δy / L`
fn next_sqrt_price_from_amount1_in(sqrt_p: u128, liquidity: u128, amount: u128) -> Option<u128> {
    sqrt_p.checked_add(mul_div(amount, Q64, liquidity)?)
}

/// Sqrt price reached after swapping `amount_in` of the input token into a
/// range with constant `liquidity`.
pub fn next_sqrt_price_from_input(
    sqrt_p: u128,
    liquidity: u128,
    amount_in: u128,
    zero_for_one: bool,
) -> Option<u128> {
    if sqrt_p == 0 || liquidity == 0 {
        return None;
    }
    if zero_for_one {
        next_sqrt_price_from_amount0_in(sqrt_p, liquidity, amount_in)
    } else {
        next_sqrt_price_from_amount1_in(sqrt_p, liquidity, amount_in)
    }
}

/// Result of swapping inside a single constant-liquidity segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapStep {
    pub sqrt_price_next: u128,
    pub amount_in: u128,
    pub amount_out: u128,
    pub fee_amount: u128,
}

/// Executes an exact-input swap from `sqrt_current` toward `sqrt_target`
/// within one tick segment of constant `liquidity`.
///
/// Invariants (proved in README §4, fuzzed in tests):
/// * `amount_in + fee_amount <= amount_remaining`
/// * the next price lies between current and target (inclusive)
/// * if the target is not reached, the whole `amount_remaining` is consumed
pub fn compute_swap_step(
    sqrt_current: u128,
    sqrt_target: u128,
    liquidity: u128,
    amount_remaining: u128,
    fee_pips: u32,
) -> Option<SwapStep> {
    let fee = fee_pips as u128;
    let zero_for_one = sqrt_current >= sqrt_target;
    let remaining_less_fee = mul_div(amount_remaining, FEE_DENOMINATOR - fee, FEE_DENOMINATOR)?;

    let in_to_target = if zero_for_one {
        amount0_delta(sqrt_target, sqrt_current, liquidity, true)
    } else {
        amount1_delta(sqrt_current, sqrt_target, liquidity, true)
    };

    // An overflowing "amount to target" is necessarily larger than anything
    // the trader can supply, so it is treated as "target not reachable".
    let (sqrt_next, reached, amount_in) = match in_to_target {
        Some(a) if remaining_less_fee >= a => (sqrt_target, true, a),
        _ => {
            let next = next_sqrt_price_from_input(
                sqrt_current,
                liquidity,
                remaining_less_fee,
                zero_for_one,
            )?;
            // Rounding can overshoot the target by at most one ulp; clamp.
            let next = if zero_for_one {
                next.max(sqrt_target)
            } else {
                next.min(sqrt_target)
            };
            let a = if zero_for_one {
                amount0_delta(next, sqrt_current, liquidity, true)?
            } else {
                amount1_delta(sqrt_current, next, liquidity, true)?
            };
            (next, next == sqrt_target, a)
        }
    };

    let amount_out = if zero_for_one {
        amount1_delta(sqrt_next, sqrt_current, liquidity, false)?
    } else {
        amount0_delta(sqrt_current, sqrt_next, liquidity, false)?
    };

    let fee_amount = if reached {
        mul_div_up(amount_in, fee, FEE_DENOMINATOR - fee)?
    } else {
        // The remainder (fee plus sub-unit rounding dust) goes to LPs.
        amount_remaining.checked_sub(amount_in)?
    };

    // Guards the `reached` branch where a rounded-up fee could exceed the
    // input actually available.
    if amount_in.checked_add(fee_amount)? > amount_remaining {
        return None;
    }

    Some(SwapStep {
        sqrt_price_next: sqrt_next,
        amount_in,
        amount_out,
        fee_amount,
    })
}

/// Largest liquidity whose token0 requirement over `[sqrt_a, sqrt_b]` is at
/// most `amount0`: `L = Δx · √Pa · √Pb / (√Pb − √Pa)`.
pub fn liquidity_for_amount0(sqrt_a: u128, sqrt_b: u128, amount0: u128) -> Option<u128> {
    let (sa, sb) = sort(sqrt_a, sqrt_b);
    if sa == sb {
        return None;
    }
    let intermediate = mul_div(sa, sb, Q64)?;
    mul_div(amount0, intermediate, sb - sa)
}

/// Largest liquidity whose token1 requirement over `[sqrt_a, sqrt_b]` is at
/// most `amount1`: `L = Δy / (√Pb − √Pa)`.
pub fn liquidity_for_amount1(sqrt_a: u128, sqrt_b: u128, amount1: u128) -> Option<u128> {
    let (sa, sb) = sort(sqrt_a, sqrt_b);
    if sa == sb {
        return None;
    }
    mul_div(amount1, Q64, sb - sa)
}

/// Maximum liquidity mintable in `[sqrt_a, sqrt_b]` at the current price
/// given token budgets (the binding constraint wins).
pub fn liquidity_for_amounts(
    sqrt_price: u128,
    sqrt_a: u128,
    sqrt_b: u128,
    amount0: u128,
    amount1: u128,
) -> Option<u128> {
    let (sa, sb) = sort(sqrt_a, sqrt_b);
    if sqrt_price <= sa {
        liquidity_for_amount0(sa, sb, amount0)
    } else if sqrt_price < sb {
        let l0 = liquidity_for_amount0(sqrt_price, sb, amount0)?;
        let l1 = liquidity_for_amount1(sa, sqrt_price, amount1)?;
        Some(l0.min(l1))
    } else {
        liquidity_for_amount1(sa, sb, amount1)
    }
}
