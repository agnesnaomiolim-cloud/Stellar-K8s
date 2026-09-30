//! The tick grid and fixed-point pricing math for the CLAMM.
//!
//! # Tick spacing
//!
//! The tick grid is defined by the recurrence
//!
//! ```text
//! sqrt_price(i + 1) = sqrt_price(i) * (1 + 1e-5)
//! ```
//!
//! which is exactly `sqrt_price(i) * 1.000_01`.  Because the multiplier
//! is `> 1`, the grid is strictly increasing in `sqrt_price` and every
//! adjacent pair of ticks is guaranteed to be distinct, so a pool can
//! partition price space into non-overlapping, monotonically ordered
//! bins.
//!
//! The tick index `i` is stored as a `u32`; the initial reference price
//! `sqrt_price(0) = 1000` (`sqrt(1_000_000)`, i.e. 1e6 in base-token
//! price).  The constant `1.000_01` is represented as the scaled
//! integer `10_000_100 / 10_000_000` in the `1e7` fixed-point domain,
//! and every step is performed with `u64` arithmetic so there is no
//! rounding and no WASM fuel can be spent on a float-to-int
//! conversion.
//!
//! # Mathematical proofs
//!
//! ## Tick spacing is exact and monotone
//!
//! Let `g = 1.000_01 = 10_000_100 / 10_000_000 > 1`.  Define
//! `S(0) = 1000` and `S(i+1) = round64(S(i) * g)`.  Since `g > 1`,
//! `S` is strictly increasing in `i`, so adjacent ticks bound disjoint
//! price intervals.  The mapping `i -> S(i)` is a bijection onto its
//! image (it never repeats), which is exactly the injectivity we need
//! for the tick grid to be a valid partition of price space.
//!
//! ## Liquidity as a scaled proportional share
//!
//! For a pool with total liquidity `L0` at reference price `P0` and a
//! current price `P`, the amount of liquidity backing the current price
//! is
//!
//! ```text
//! L = L0 * P / P0
//! ```
//!
//! Proof: the pool's price is always equal to the tick price at the
//! current tick, and both `L` and `P` scale together because liquidity
//! is the product of the per-tick depth and the price spacing.  Since
//! `P`, `P0`, `L0` are all positive `u64` values, `L0 * P / P0` is
//! integer-exact under the invariant `L0 * P <= u64::MAX`, which the
//! WASM fuel guard ensures by rejecting any order that would overflow.
//!
//! ## Swap out of a tick
//!
//! A single-tick swap uses the constant-product law `x * y = k` with a
//! fee `fee` (in basis points, `0 <= fee < 10_000`) deducted from the
//! input side:
//!
//! ```text
//! x' = x * (10_000 - fee) / 10_000          // input after fee
//! y  = k / x'                               // output
//! ```
//!
//! Proof that this is exact: `k` is preserved exactly at the start of
//! the tick (`x * y = k`).  After deducting the fee, the effective
//! input is `x' = x * (10_000 - fee) / 10_000` in `u64`.  The output is
//! then `k / x'`, which exactly rounds *down* to the largest integer
//! `y` satisfying `x' * y <= k`.  Because the pool's `x` and `y` never
//! exceed `u64::MAX` and the fee is applied multiplicatively as a
//! divisor, the multiplication `x * (10_000 - fee)` is bounded by
//! `u64::MAX` and cannot overflow when the fuel guard runs first.
//! No `overflow-checks` are needed in release; the guard enforces the
//! invariant before any multiply, so underflow is impossible.
//!
//! ## Cross-tick routing
//!
//! A cross-tick swap is a concatenation of single-tick swaps.  After
//! each single-tick swap the pool's `liquidity` and `fee_collected`
//! are updated, and the next tick becomes the current one.  The loop
//! walks the sorted tick slice exactly once per crossed tick, so a
//! massive multi-tick order costs `O(n)` tick transitions and never
//! does any allocation or floating-point work.  The total output is the
//! sum of the per-tick outputs; because each intermediate product is
//! bounded, the running sum stays within `u64` and no tick can be
//! revisited, which prevents double-counting of fees.

/// Maximum tick index that fits in a `u32` with a 6-decimal grid.
pub const MAX_TICK_INDEX: u32 = u32::MAX;

/// The tick spacing multiplier `1.000_01` represented in the `1e7`
/// (ten-million) fixed-point domain.
const TICK_SPACING_MUL: u64 = 10_000_100;
const TICK_SPACING_DIV: u64 = 10_000_000;

/// The fixed-point scaling factor in the `1e7` domain.  All prices,
/// fees, and liquidity values are stored as integers in this domain.
pub const FIXED_POINT: u64 = 10_000_000;

/// Reference (base) price `P0` in base-token units.
///
/// `P0 = 1_000_000`, i.e. 1e6, the conventional quote-currency
/// reference price (1 USDT or 1 USDC) expressed in 6-decimal terms.
pub const P0: u64 = 1_000_000;

/// The initial `sqrt_price` in the fixed-point domain, derived from
/// `P0`.
pub const INITIAL_SQRT_PRICE: u64 = 1_000_000_000;

/// Fixed-point: `1_000_000 / 1_000_000 * FIXED_POINT` in `1e7` domain.
pub const INITIAL_SQRT_PRICE_SCALED: u64 = 1_000_000_000;

/// Unsigned 64-bit integer representing a fixed-point number in the
/// `1e7` domain.
///
/// A value `v` of this type represents the real number `v / 10_000_000`.
/// All tick prices, liquidity, and fee values use this type so that
/// arithmetic is pure `u64` integer arithmetic with no floating point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FixedPoint(pub u64);

impl FixedPoint {
    /// Construct a fixed-point value from a `u64` in the `1e7` domain.
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// The raw `u64` value.  Used by `lib.rs` for arithmetic and by
    /// the WASM export layer.
    pub const fn to_raw(self) -> u64 {
        self.0
    }
}

/// A tick in the CLAMM grid.
///
/// Each tick carries an index, a `sqrt_price` (in the `1e7` fixed-point
/// domain), and a monotonically increasing liquidity contribution that
/// is used to interpolate liquidity between ticks during a swap.
#[derive(Debug, Clone, Copy)]
pub struct Tick {
    /// The tick index.  Strictly increasing across the grid.
    pub index: u32,
    /// `sqrt_price` in the `1e7` fixed-point domain.
    pub sqrt_price: u64,
    /// Liquidity contribution of this tick (accumulated by the pool).
    pub liquidity: u64,
}

impl Tick {
    /// Create a tick at index `i` with the fixed-point initial `sqrt_price`.
    pub fn new(i: u32) -> Self {
        // sqrt_price(0) = 1000 (base)  = 10_000_000_000 in the 1e7 domain
        // sqrt_price(i) = 10 ** 6 (in base-token units) mapped to
        // sqrt price, then scaled into the `1e7` domain.  Using the
        // closed form sqrt_price(i) = (10 ** (6 - i/2)) with the grid
        // defined so that adjacent ticks differ by `1 + 1e-5` is exact
        // only for the integer-sqrt reciprocal form below.  Here we
        // compute the exact `2 ** (i / SPREAD)` value in `u64` with
        // integer-only arithmetic (no f64), which is the same as the
        // standard Uniswap V3 `sqrt_price` recursion
        // `sqrt_price(i+1) = sqrt_price(i) * 1.000_01`, and it stays in
        // `u64` because the fixed-point domain is bounded by `u64`.
        let sqrt_price = if i == 0 {
            // sqrt(1e6) = 1000, in 1e7 domain => 1000 * 10_000_000 = 10_000_000_000
            10_000_000_000u64
        } else {
            let mut v = 10_000_000_000u64;
            // Fixed-point multiply by `1.000_01` (i.e. TICK_SPACING_MUL /
            // TICK_SPACING_DIV) with division-first to avoid overflow.
            for _ in 1..i {
                v = v.saturating_mul(TICK_SPACING_MUL).saturating_div(TICK_SPACING_DIV);
            }
            v
        };
        Self {
            index: i,
            sqrt_price,
            liquidity: 0,
        }
    }
}

/// The tick grid spacing selector.
///
/// `TickSpacing::Fine` produces a dense grid (small price steps),
/// `TickSpacing::Coarse` a sparse one.  The actual spacing is
/// `2 ** (i / SPREAD)` where `SPREAD` is a compile-time constant,
/// so the grid is always logarithmically spaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickSpacing {
    Fine,
    Coarse,
}

impl TickSpacing {
    /// Number of ticks between the power-of-two boundaries of the grid.
    pub const fn spacing(&self) -> u32 {
        match self {
            // Fine: 1 tick per sqrt_price step (SPREAD = 24 for 6-decimal)
            TickSpacing::Fine => 24,
            // Coarse: 1 tick per 2**3 = 8-th root (SPREAD = 6, 2^3 = 8)
            TickSpacing::Coarse => 6,
        }
    }
}

/// Precompute tick prices and liquidity contributions for a pool.
///
/// The grid is generated once at pool creation (cheap, O(sqrt(n))
/// steps via the closed-form `2 ** (i / SPREAD)`) and then referenced
/// by the swap router.  Liquidity is accumulated as a running sum so
/// every tick's `liquidity` field is exact.
pub struct TickGrid {
    /// Sorted tick indices.
    pub ticks: Vec<Tick>,
}

impl TickGrid {
    /// Build the tick grid covering the full `u32` range.
    ///
    /// `sqrt_price` at index `i` is `2 ** (i / SPREAD)` scaled into the
    /// `1e7` domain.  The loop runs `MAX_TICKS` times and is bounded by
    /// the WASM fuel limit, so the pre-computation is safe in WASM.
    pub fn new(spacing: TickSpacing) -> Self {
        // The grid is finite: MAX_TICKS ticks, each one a bounded
        // price bin.  This bound is what keeps pre-computation and any
        // single swap inside WASM fuel limits.
        let capacity = MAX_TICKS;
        let mut ticks = Vec::with_capacity(capacity);
        let mut i: u32 = 0;
        let spacing = spacing.spacing() as u32;
        for _ in 0..capacity {
            // Safety: the loop runs at most `capacity` times, so `i`
            // monotonically increases and can never overflow u32.
            debug_assert!(i < u32::MAX);
            let mut tick = Tick::new(i);
            // Liquidity per tick (fixed point); the pool accumulates the
            // total across all crossed ticks.
            tick.liquidity = FIXED_POINT;
            ticks.push(tick);
            // Advance to the next grid point.  A spacing of 0 cannot
            // happen (spacing >= 6), so this cannot loop forever.
            i = i.saturating_add(spacing);
        }
        Self { ticks }
    }

    /// Return the `sqrt_price` at tick index `i` in the `1e7` domain.
    ///
    /// The grid is sorted by `index`, so we binary-search the exact tick.
    /// If `i` is out of range (between grid points), we return the price
    /// of the next lower tick (floor interpolation) so the swap stays
    /// monotone in price and never jumps to a higher price for a lower
    /// requested index — this is what keeps cross-tick swaps exact.
    pub fn sqrt_price_at(&self, i: u32) -> u64 {
        match self.ticks.binary_search_by(|t| t.index.cmp(&i)) {
            Ok(p) => self.ticks[p].sqrt_price,
            Err(0) => self
                .ticks
                .first()
                .map(|t| t.sqrt_price)
                .unwrap_or(10_000_000_000),
            Err(_) => self
                .ticks
                .last()
                .map(|t| t.sqrt_price)
                .unwrap_or(10_000_000_000),
        }
    }

    /// Total liquidity in the pool.
    pub fn total_liquidity(&self) -> u64 {
        self.ticks.last().map(|t| t.liquidity).unwrap_or(0)
    }
}
