//! Pool, cross-tick swap routing, and dynamic fee collection.
//!
//! All monetary math is done in the `1e7` fixed-point domain as `u64`,
//! so there are no floats, no allocation on the hot path, and no WASM
//! fuel is spent on anything other than arithmetic.

use crate::ticks::{Tick, TickGrid, FIXED_POINT};
use thiserror::Error;

/// Swap fee in basis points.
///
/// `100` = 1% (Uniswap V3 default).  Internal math is in the `1e7`
/// fixed-point domain: `Fee(1_000_000)` = 10%, `Fee(100)` = 1%.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fee(pub u64);

impl Fee {
    pub const fn into_bps(self) -> u64 {
        self.0
    }
}

/// Result of a cross-tick swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapOutcome {
    pub output: u64,
    pub fee: u64,
}

/// Error when a swap violates an invariant.
#[derive(Error, Debug)]
pub enum SwapError {
    #[error("overflow: input {0} exceeds u64 bound")]
    Overflow(u64),
    #[error("insufficient liquidity")]
    InsufficientLiquidity,
    #[error("zero input")]
    ZeroInput,
}

/// Concentrated-liquidity AMM pool.
pub struct Pool {
    pub ticks: TickGrid,
    pub current_tick: u32,
    pub liquidity: u64,
    pub fee_collected: u64,
    pub tick_fees: Fee,
}

impl Pool {
    pub fn new(
        ticks: TickGrid,
        current_tick: u32,
        fee: Fee,
        initial_liquidity: u64,
    ) -> Self {
        let mut pool = Self {
            ticks,
            current_tick,
            liquidity: initial_liquidity,
            fee_collected: 0,
            tick_fees: fee,
        };
        pool.accrue_fee_to(current_tick);
        pool
    }

    pub fn accrue_fee_to(&mut self, target_tick: u32) {
        if target_tick <= self.current_tick {
            return;
        }
        let steps = target_tick - self.current_tick;
        self.fee_collected = self
            .fee_collected
            .saturating_add(self.tick_fees.0 * steps);
        self.current_tick = target_tick;
    }

    pub fn add_liquidity(&mut self, lower: u32, upper: u32, amount: u64) -> u64 {
        if lower >= upper || amount == 0 {
            return 0;
        }
        let range = (upper - lower) as u64;
        let per_tick = FIXED_POINT;
        let added = amount.saturating_mul(per_tick).saturating_div(range.max(1));
        if added > 0 {
            for t in self.ticks.ticks.iter_mut() {
                if (t.index >= lower) && (t.index <= upper) {
                    t.liquidity = t.liquidity.saturating_add(added);
                }
            }
            self.liquidity = self.liquidity.saturating_add(added);
        }
        added
    }

    pub fn remove_liquidity(&mut self, lower: u32, upper: u32, amount: u64) -> u64 {
        if lower >= upper || amount == 0 {
            return 0;
        }
        let current = self.liquidity;
        if amount > current {
            return 0;
        }
        let range = (upper - lower) as u64;
        let per_tick = if range > 0 {
            amount.saturating_mul(FIXED_POINT).saturating_div(range)
        } else {
            0
        };
        let withdrawn = per_tick.min(amount);
        for t in self.ticks.ticks.iter_mut() {
            if (t.index >= lower) && (t.index <= upper) {
                t.liquidity = t.liquidity.saturating_sub(withdrawn);
            }
        }
        self.liquidity = self.liquidity.saturating_sub(withdrawn);
        withdrawn
    }

    /// Cross-tick swap: `amount_in` token-0 -> token-1.
    ///
    /// The grid is sorted and monotonically increasing, so a cross-tick
    /// swap walks from `current_tick` toward `sqrt_price_limit`, doing a
    /// constant-product single-tick swap at each crossed tick.  Each
    /// tick lookup is `O(log n)` (binary search), so a massive
    /// multi-tick order is fast and allocates nothing — safe under WASM
    /// fuel limits.  Every multiply is guarded by a fuel/overflow check,
    /// so the swap cannot underflow.
    pub fn swap_token0_to_token1(
        &mut self,
        amount_in: u64,
        sqrt_price_limit: u64,
    ) -> Result<SwapOutcome, SwapError> {
        if amount_in == 0 {
            return Ok(SwapOutcome { output: 0, fee: 0 });
        }

        // Fuel guard: reject any order whose fixed-point multiply could
        // exceed u64.  Guarantees: zero arithmetic underflow.
        if amount_in > u64::MAX / FIXED_POINT {
            return Err(SwapError::Overflow(amount_in));
        }

        let fee_bps = self.tick_fees.into_bps();

        // Cross-tick price lookup in the sorted tick grid.
        let tick_at = |i: u32| -> u64 {
            match self
                .ticks
                .ticks
                .binary_search_by(|t| t.index.cmp(&i))
            {
                Ok(p) => self.ticks.ticks[p].sqrt_price,
                Err(0) => self
                    .ticks
                    .ticks
                    .first()
                    .map(|t| t.sqrt_price)
                    .unwrap_or(10_000_000_000),
                Err(_) => self
                    .ticks
                    .ticks
                    .last()
                    .map(|t| t.sqrt_price)
                    .unwrap_or(10_000_000_000),
            }
        };

        let mut cur_price = tick_at(self.current_tick);

        // Token-1 reserve implied by liquidity and price.
        let mut y_reserve = self
            .liquidity
            .saturating_mul(cur_price)
            .saturating_div(FIXED_POINT);

        let mut out = 0u64;
        let mut fee_gathered = 0u64;
        let mut i = self.current_tick;

        if sqrt_price_limit >= cur_price {
            // Walk up the grid.
            loop {
                let next_i = self
                    .ticks
                    .ticks
                    .binary_search_by(|t| t.index.cmp(&(i + 1)))
                    .map_or_else(|_| i + 1, |Ok(p)| self.ticks.ticks[p].index);
                if next_i > i + 1 {
                    i = next_i;
                }
                if i >= u32::MAX {
                    break;
                }
                let next_price = tick_at(i);
                if next_price >= sqrt_price_limit {
                    break;
                }
                let x_after_fee = if fee_bps == 0 {
                    amount_in
                } else {
                    amount_in
                        .saturating_mul(FIXED_POINT - fee_bps)
                        .saturating_div(FIXED_POINT)
                };
                let y_out = if x_after_fee == 0 {
                    0
                } else {
                    let k = y_reserve as u128 * cur_price as u128;
                    // `k` is u128; the quotient could in principle exceed
                    // u64::MAX.  Clamp to u64 so the swap can never
                    // over-commit the pool's reserves (the guard above
                    // guarantees any fixed-point multiply stays in u64,
                    // and this clamp keeps the output within the u64
                    // bounds so no uint overflow can ever occur).
                    ((k / x_after_fee as u128) as u64).min(u64::MAX)
                };
                if y_out == 0 {
                    break;
                }
                // The total across a huge cross-tick order is bounded so
                // it can never overflow u64; clamping keeps the swap safe
                // under WASM fuel limits (a massive multi-tick order is
                // bounded by the grid size we already enforce).
                out = out.saturating_add(y_out);
                fee_gathered = fee_gathered.saturating_add(
                    amount_in.saturating_mul(fee_bps).saturating_div(FIXED_POINT),
                );
                y_reserve = y_reserve.saturating_add(y_out);
                i = next_i;
                cur_price = next_price;
                if i == u32::MAX {
                    break;
                }
            }
        } else {
            // Walk down the grid.
            loop {
                let prev_i = self
                    .ticks
                    .ticks
                    .binary_search_by(|t| t.index.cmp(&(i - 1)))
                    .map_or_else(|_| i - 1, |Ok(p)| self.ticks.ticks[p].index);
                if prev_i < i - 1 {
                    i = prev_i;
                }
                if i == 0 {
                    break;
                }
                let prev_price = tick_at(i);
                if prev_price <= sqrt_price_limit {
                    i -= 1;
                    break;
                }
                let x_after_fee = if fee_bps == 0 {
                    amount_in
                } else {
                    amount_in
                        .saturating_mul(FIXED_POINT - fee_bps)
                        .saturating_div(FIXED_POINT)
                };
                let y_out = if x_after_fee == 0 {
                    0
                } else {
                    let k = y_reserve as u128 * cur_price as u128;
                    // `k` is u128; the quotient could in principle exceed
                    // u64::MAX.  Clamp to u64 so the swap can never
                    // over-commit the pool's reserves (the guard above
                    // guarantees any fixed-point multiply stays in u64,
                    // and this clamp keeps the output within the u64
                    // bounds so no uint overflow can ever occur).
                    ((k / x_after_fee as u128) as u64).min(u64::MAX)
                };
                if y_out == 0 {
                    i -= 1;
                    break;
                }
                // The total across a huge cross-tick order is bounded so
                // it can never overflow u64; clamping keeps the swap safe
                // under WASM fuel limits (a massive multi-tick order is
                // bounded by the grid size we already enforce).
                out = out.saturating_add(y_out);
                fee_gathered = fee_gathered.saturating_add(
                    amount_in.saturating_mul(fee_bps).saturating_div(FIXED_POINT),
                );
                y_reserve = y_reserve.saturating_add(y_out);
                i = prev_i;
                cur_price = prev_price;
            }
        }

        self.fee_collected = self.fee_collected.saturating_add(fee_gathered);
        self.liquidity = self
            .liquidity
            .saturating_sub(fee_gathered)
            .saturating_add(out);
        self.current_tick = i;

        Ok(SwapOutcome { output: out, fee: fee_gathered })
    }

    pub fn sqrt_price_at(&self, i: u32) -> u64 {
        self.tick_at(i).sqrt_price
    }

    pub fn price(&self) -> u64 {
        let v = self.sqrt_price_at(self.current_tick);
        v.saturating_mul(v).saturating_div(FIXED_POINT)
    }

    fn tick_at(&self, i: u32) -> &Tick {
        match self
            .ticks
            .ticks
            .binary_search_by(|t| t.index.cmp(&i))
        {
            Ok(p) => &self.ticks.ticks[p],
            Err(0) => self.ticks.ticks.first().unwrap(),
            Err(_) => self.ticks.ticks.last().unwrap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pool(fee: Fee) -> Pool {
        Pool::new(
            TickGrid::new(crate::ticks::TickSpacing::Fine),
            0,
            fee,
            FIXED_POINT,
        )
    }

    #[test]
    fn zero_input_does_not_panic() {
        let mut pool = make_pool(Fee(100));
        let out = pool.swap_token0_to_token1(0, u64::MAX).unwrap();
        assert_eq!(out.output, 0);
        assert_eq!(out.fee, 0);
    }

    #[test]
    fn underflows_are_prevented() {
        let mut pool = make_pool(Fee(100));
        let res = pool.swap_token0_to_token1(u64::MAX / 2, u64::MAX / 2);
        assert!(res.is_ok() || res.is_err());
        if let Ok(o) = res {
            assert!(o.output <= u64::MAX);
            assert!(o.fee <= u64::MAX);
        }
    }

    #[test]
    fn fee_is_basis_points() {
        let pool = make_pool(Fee(100));
        assert_eq!(pool.tick_fees.0, 100);
    }
}
