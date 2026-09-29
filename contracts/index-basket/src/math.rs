//! Fixed-point helpers. Every quantity handled here is non-negative.
//!
//! Products are computed in `i128` when they fit and fall back to the host's
//! 256-bit integers otherwise, so 18-decimal tokens with large balances and
//! high-precision oracle prices never overflow an intermediate result.

use core::cmp::Ordering;

use soroban_sdk::{panic_with_error, Env, U256};

use crate::Error;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    Down,
    Up,
}

pub fn pow10(env: &Env, exp: u32) -> i128 {
    10i128
        .checked_pow(exp)
        .unwrap_or_else(|| panic_with_error!(env, Error::Overflow))
}

fn u256(env: &Env, v: i128) -> U256 {
    if v < 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
    U256::from_u128(env, v as u128)
}

fn to_i128(env: &Env, v: U256) -> i128 {
    v.to_u128()
        .and_then(|v| i128::try_from(v).ok())
        .unwrap_or_else(|| panic_with_error!(env, Error::Overflow))
}

/// `a * b / d`, rounded as requested.
pub fn mul_div(env: &Env, a: i128, b: i128, d: i128, rounding: Rounding) -> i128 {
    if a < 0 || b < 0 || d <= 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
    if let Some(p) = a.checked_mul(b) {
        let q = p / d;
        return if rounding == Rounding::Up && p % d != 0 {
            q + 1
        } else {
            q
        };
    }
    div_u256(env, u256(env, a).mul(&u256(env, b)), u256(env, d), rounding)
}

/// `(n0 * n1 * ...) / (d0 * d1 * ...)` evaluated in 256-bit precision with a
/// single final rounding.
pub fn mul_div_many(env: &Env, nums: &[i128], dens: &[i128], rounding: Rounding) -> i128 {
    let mut n = U256::from_u32(env, 1);
    for v in nums {
        n = n.mul(&u256(env, *v));
    }
    let mut d = U256::from_u32(env, 1);
    for v in dens {
        if *v <= 0 {
            panic_with_error!(env, Error::InvalidAmount);
        }
        d = d.mul(&u256(env, *v));
    }
    div_u256(env, n, d, rounding)
}

fn div_u256(env: &Env, n: U256, d: U256, rounding: Rounding) -> i128 {
    let mut q = n.div(&d);
    if rounding == Rounding::Up && n.rem_euclid(&d) != U256::from_u32(env, 0) {
        q = q.add(&U256::from_u32(env, 1));
    }
    to_i128(env, q)
}

/// Compares `a * x` with `b * y` without overflow.
pub fn cmp_products(env: &Env, a: i128, x: i128, b: i128, y: i128) -> Ordering {
    match (a.checked_mul(x), b.checked_mul(y)) {
        (Some(l), Some(r)) if a >= 0 && x >= 0 && b >= 0 && y >= 0 => l.cmp(&r),
        _ => u256(env, a)
            .mul(&u256(env, x))
            .cmp(&u256(env, b).mul(&u256(env, y))),
    }
}

/// Value of `amount` base units of a `decimals`-precision token at `price`
/// (quote units per whole token, in the oracle's scale).
pub fn value(env: &Env, amount: i128, price: i128, decimals: u32) -> i128 {
    mul_div(env, amount, price, pow10(env, decimals), Rounding::Down)
}
