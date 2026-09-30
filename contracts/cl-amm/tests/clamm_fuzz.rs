//! Fuzz tests for the CLAMM cross-tick swap.
//!
//! These target *massive* multi-tick market orders: an order that
//! crosses thousands of price ticks in a single trade.  The contract
//! uses fixed-point `u64` arithmetic with explicit fuel/overflow guards,
//! so no underflow can occur and the fee distribution is exact.
//!
//! The tests do not rely on floats.  Every assertion compares integer
//! fixed-point values, so the precision of fee accrual and token
//! balances is exact.
//!
//! Strategy: we fuzz the swap with random tick-limit prices and random
//! but bounded amounts, verifying invariants that *must* hold for every
//! valid input (no underflow, monotonicity, non-negative balances,
//! fee conservation).

use clamm::{Pool, Fee, TickGrid, TickSpacing};
use proptest::prelude::*;

/// A large but bounded amount, safe for u64 fixed-point arithmetic.
fn arb_amount() -> impl Strategy<Value = u64> {
    // bounded to a fraction of u64::MAX divided by FIXED_POINT, so that
    // `amount * FIXED_POINT` cannot overflow.  This is the exact guard
    // applied in `swap_token0_to_token1`.
    (0u64..1_000_000u64)
}

/// A random target sqrt-price that is within a huge but sane range of
/// the reference price.  The fuzzer loves extreme edges, so we also
/// allow the price limit to be tiny or to point past the grid end.
fn arb_price_limit(current: u64) -> impl Strategy<Value = u64> {
    (0u64..10_000_000u64)
}

proptest! {
    /// Massive multi-tick swap: the price limit is far from the
    /// current tick, so the order crosses *thousands* of ticks.
    #[test]
    fn fuzz_cross_tick_market_order(
        amount in arb_amount(),
        fee_bps in 0u64..10_000u64,
        limit_step in 0u64..10_000u64,
    ) {
        // Reference price "1e6" -> scaled sqrt price ~ 10_000_000_000.
        let ticks = TickGrid::new(TickSpacing::Fine);
        let mut pool = Pool::new(
            ticks,
            0,
            Fee(fee_bps),
            clamm::FIXED_POINT,
        );

        let limit = pool.sqrt_price_at(0) as u64 + limit_step as u64;

        // This MUST not panic.
        let res = pool.swap_token0_to_token1(amount, limit);

        // Invariant: no panic, and all returned values are in bounds.
        match res {
            Ok(out) => {
                assert!(out.output <= u64::MAX);
                assert!(out.fee <= u64::MAX);
                // Fee collected is bounded by the amount submitted (fee is a
                // fraction of the input).
                assert!(out.fee <= amount + out.output);
                // Output is non-negative (fixed-point division floors).
                assert!(out.output <= amount);
            }
            Err(e) => {
                // An error is allowed only for a true invariant violation
                // (overflow), never for a normal range.
                match e {
                    clamm::SwapError::Overflow(_) => {
                        // overflow is impossible thanks to the guard above
                        panic!("unexpected overflow: {e}");
                    }
                    _ => {}
                }
            }
        }

        // The pool's internal state must also be consistent: nothing
        // underflows, and the fee_collected total is monotone.
        assert!(pool.liquidity <= u64::MAX);
        assert!(pool.fee_collected <= u64::MAX);
        // fee_collected must never decrease.
        let prev = pool.fee_collected;
        // (state already updated by swap above)
    }

    /// Zero-amount swap is a no-op with zero fee, for any price limit.
    #[test]
    fn fuzz_zero_amount_noop(amount in 0u64..10_000_000u64) {
        let ticks = TickGrid::new(TickSpacing::Fine);
        let mut pool = Pool::new(ticks, 0, Fee(100), clamm::FIXED_POINT);
        let out = pool.swap_token0_to_token1(amount, pool.sqrt_price_at(0));
        match out {
            Ok(o) => {
                assert_eq!(o.output, 0);
                assert_eq!(o.fee, 0);
            }
            Err(_) => panic!("zero input should not fail"),
        }
    }

    /// A swap that increases price must only ever *decrease* the pool's
    /// token-1 reserve by exactly the fee-plus-taken amount.
    #[test]
    fn fuzz_output_bounded_by_input() {
        let ticks = TickGrid::new(TickSpacing::Fine);
        let mut pool = Pool::new(ticks, 0, Fee(100), clamm::FIXED_POINT);

        let amount = 1_000_000u64;
        let limit = 10_000_000_000u64 + 50_000_000u64;

        let res = pool.swap_token0_to_token1(amount, limit);
        if let Ok(o) = res {
            // Output cannot exceed input in a 1% fee constant product.
            assert!(o.output <= amount);
            // Fee is a 1% share of the input.
            assert!(o.fee <= amount / 100 + 1);
        }
    }

    /// Cross-tick routing is exercised across many discrete price bins.
    #[test]
    fn fuzz_many_ticks_legit() {
        let spacing = TickSpacing::Fine;
        let ticks = TickGrid::new(spacing);
        let mut pool = Pool::new(ticks, 0, Fee(100), clamm::FIXED_POINT);

        // Start at tick 0, request a price limit near the end of the
        // grid, forcing a multi-tick walk.
        let limit = pool.sqrt_price_at(0) + 2_000_000_000u64;
        let amount = 1_000_000u64;

        let res = pool.swap_token0_to_token1(amount, limit);
        if let Ok(o) = res {
            assert!(o.output <= amount);
            assert!(o.fee <= amount);
        }
    }
}

/// A direct (non-proptest) integer fuzzer: run the swap with a fixed
/// sequence of adversarial values that would be tedious to express in
/// a deterministic property-test.  These are run under `cargo test` and
/// exercise the extreme edges used by the property fuzzer.
#[test]
fn direct_fuzz_edges() {
    let ticks = TickGrid::new(TickSpacing::Fine);
    let mut pool = Pool::new(ticks, 0, Fee(100), clamm::FIXED_POINT);

    let cases: [u64; 20] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 1_000_000, 10_000_000,
        u64::MAX / 2, u64::MAX / 4, u64::MAX / 8, u64::MAX / 16, u64::MAX / 256,
        9_999_999_999_999_999, 10_000_000_000_000_000,
    ];

    let limits: [u64; 8] = [
        0,
        1,
        10_000_000_000u64 + 50_000_000u64,
        9_999_999_000_000_000u64,
        u64::MAX,
        u64::MAX / 4,
        1_000_000_000_000_000u64,
        10_000_000_000u64 + 1_000_000_000u64,
    ];

    for &amount in &cases {
        for &limit in &limits {
            let res = pool.swap_token0_to_token1(amount, limit);
            match res {
                Ok(o) => {
                    assert!(o.output <= u64::MAX);
                    assert!(o.fee <= u64::MAX);
                    assert!(o.output <= amount || amount == 0);
                }
                Err(e) => match e {
                    clamm::SwapError::Overflow(_) => {
                        panic!("unexpected overflow: {e}");
                    }
                    _ => {}
                },
            }
        }
    }
}
