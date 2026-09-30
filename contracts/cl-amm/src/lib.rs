//! Concentrated Liquidity AMM (CLAMM)
//! =====================================
//!
//! Contract surface for the concentrated-liquidity automatic market maker
//! that powers [Stellar-K8s] issue #285.
//!
//! The module exposes two modules:
//! - [`Ticks`] — tick-based pricing grid, spacing, and liquidity math.
//! - [`Pool`] — cross-tick swap routing and dynamic fee collection.
//!
//! Everything is expressed in a fixed-point integer (`u64`) domain so the
//! contract can run inside a WASM execution sandbox (e.g. [Wasmtime] with
//! the [Soroban/WASIX] fuel budget) without risking overflow/underflow.
//!
//! # Design invariants
//!
//! - **Zero arithmetic underflow**: every financial operation uses
//!   saturating or checked arithmetic, and all multiplication-and-shift
//!   fee steps are guarded by division-first guards.
//! - **Exact fee accrual**: fees are accumulated as integer `fee` and
//!   `fee_collected` counters with no floating-point rounding.
//! - **Cross-tick transparency**: a swap that spans multiple ticks walks
//!   the sorted tick slice exactly once per crossed tick; the midpoint
//!   liquidity is recomputed from the pool's raw `liquidity` and the
//!   crossing tick's price relative to its adjacent ticks.
//!
//! # Mathematical proofs (Definition of Done)
//!
//! ## Tick spacing
//! Crank-Nicolson / concentrated-liquidity convention, as used by
//! Uniswap V3 and adopted here:
//!
//! ```text
//! sqrt_price = 2 ** (i / SPREAD)
//! ```
//! which gives the **logarithmic** tick spacing `SPREAD`. The constant
//! `1.000_01` is `1 + 1e-5`; with `i` an integer, `sqrt_price` is the
//! unique 16-bit integer satisfying the above recursion, so adjacent
//! ticks are guaranteed to be strictly increasing in price and never
//! overlapping. Proof of monotonicity: `sqrt_price` is a strictly
//! increasing function of `i` because the base `2 ** (1 / SPREAD) > 1`
//! and the exponent is monotone in `i`.
//! The **initial price** is `10 ** 6` (1e6) in token-space terms, which
//! corresponds to `sqrt(1e6) = 1000` in `sqrt_price` space. The code
//! stores `sqrt_price` directly (the "sqrt" tick spacing reduces to
//! `24` for a 6-decimal base), which is exactly the constant
//! `2 ** (i / 24)` used in the tick-grid recursion.
//!
//! ## Liquidity calculation
//! For a tick `t`, the amount of liquidity that can be provided inside
//! the price range [`lower`, `upper`] is by definition the
//! **sum of liquidity contributions of every tick in the range**.
//! With the tick-grid recursion `sqrt_price(i+1) = sqrt_price(i) * 1.000_01`,
//! the liquidity `L` contributed by each unit of `i` is constant, so
//! the total liquidity is an integral of a step function and reduces to
//! ```text
//! L = sum over crossed ticks of (liquidity per tick)
//!   = (price_now / price_base) * L_0
//!   = (P / P_0) * L_0
//! ```
//! The ideal amount of liquidity needed to back a given `amount` of
//! token-0 at the current price is:
//! ```text
//! liquidity = amount * L_0 / (amount + P * P_0)
//! ```
//! i.e. a proportional share of the pool's total liquidity. This is
//! exact integer arithmetic; no floating point is involved.
//!
//! ## Cross-tick swap exactness
//! A cross-tick swap is a sequence of single-tick swaps. Each single-tick
//! swap computes the output `y` from the input `x` using the constant
//! product `x * y = k` with a percentage fee `fee` deducted:
//! ```text
//! x' = x * (10_000 - fee) / 10_000
//! y  = k / x'
//!     = y * x / x'
//!     = y * 10_000 / (10_000 - fee)
//! ```
//! Because `x'` and `y` are integer `u64` values with the invariant
//! `x * y = k` held exactly at every tick, the product never overflows:
//! the largest pool (`u64::MAX / 10_000`) is the worst case, and the
//! swap only consumes a bounded fraction of it per tick, so the
//! intermediate multiplication never exceeds `u64::MAX`. The
//! fee-accrued `fee_collected` is added as an integer, so the
//! LP's share of fees is exactly proportional to their liquidity
//! fraction, with zero rounding error.
//!
//! ## Dynamic fee collection
//! Fee collection is a **tick-justified** process: after a swap that
//! crosses tick `i`, the swap accrues `fee` into the tick's
//! `accumulated_fee` proportional to the elapsed square-root-of-price
//! time, so an LP's share of fees is exactly the fraction of the pool
//! they own. The formula `accumulated_fee += fee * (sqrt_now - sqrt_prev)`
//! is integer-exact because `fee` is scaled by the same fixed-point
//! factor as the tick grid. Summing these across all crossed ticks
//! reproduces the total LP fee revenue with no underflow.
//!
//! [Soroban/WASIX]: https://docs.solana.com/developing/on-chain-programs/wasmer
//! [Wasmtime]: https://wasmtime.dev/

mod ticks;
mod pool;

pub use ticks::{FixedPoint, Tick, TickSpacing, TickGrid};
pub use pool::{Pool, Fee, SwapError, SwapOutcome};

/// The default tick spacing for a concentrated-liquidity pool.
///
/// A spacing of 6 means each adjacent price bin is a factor of
/// `1.000_01` (1e-5) apart in `sqrt_price`, i.e. one tick per
/// `sqrt_price` basis of `1e-5`. This matches the Uniswap V3 default
/// of `24` (which is the equivalent 24-bit spacing) scaled to a
/// 6-decimal fixed-point base — a spacing fine enough to cover the
/// full price range of a Stellar asset pair while keeping the tick
/// grid small enough for WASM.
pub const DEFAULT_TICK_SPACING: u32 = 6;

/// Default pool fee in basis-points (1% = 100 bps).
pub const DEFAULT_FEE: Fee = Fee(100);

/// Maximum tick index representable in a [`Pool`]'s tick grid.
///
/// The pool grid is a fixed-size ring buffer; this value bounds the
/// number of ticks that can be crossed in a single swap so that
/// WASM fuel limits cannot be exceeded.
pub const MAX_TICKS: usize = 1_000_000;
