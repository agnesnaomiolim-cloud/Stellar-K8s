# Concentrated Liquidity AMM (`cl-amm`)

A tick-based concentrated liquidity AMM for Soroban. LPs provide liquidity inside
custom price ranges; swaps execute bin by bin and cross as many ticks as an order
needs; fees go only to the liquidity that is active while a swap executes.

| File | Purpose |
|------|---------|
| `src/lib.rs` | Contract: `initialize`, `mint`, `burn`, `collect`, `swap`, views |
| `src/ticks.rs` | Tick ↔ √price, tick state, bitmap search, fee-growth-inside |
| `src/math.rs` | 256-bit-intermediate fixed point, liquidity amounts, swap step |
| `src/test.rs` | Bigint-referenced math tests, contract tests, proptest fuzzing |

## API

| Function | Description |
|----------|-------------|
| `initialize(token0, token1, fee_pips, tick_spacing, sqrt_price)` | One-time pool setup. `fee_pips ≤ 100 000` (10%), `1 ≤ tick_spacing ≤ 16 384`. |
| `mint(owner, lower, upper, liquidity, amount0_max, amount1_max)` | Adds liquidity; pulls the required amounts (rounded up). |
| `burn(owner, lower, upper, liquidity)` | Removes liquidity; principal (rounded down) + fees become owed. `liquidity = 0` checkpoints fees. |
| `collect(owner, lower, upper, amount0_max, amount1_max)` | Pays out owed tokens. |
| `swap(sender, zero_for_one, amount_in, sqrt_price_limit, min_amount_out)` | Exact-input swap across ticks; `sqrt_price_limit = 0` means no limit. Only consumed input is charged. |
| `pool()`, `config()`, `tick(t)`, `position(owner, lower, upper)`, `liquidity_for_amounts(...)` | Views. |

## Building and testing

```bash
cd contracts/cl-amm
cargo build --target wasm32v1-none --release   # also enables the WASM budget test
cargo test --release
PROPTEST_CASES=1000 cargo test --release fuzz   # longer fuzz run
```

---

## Mathematical model

Notation: `P = token1/token0` price, `√P` its square root, `L` liquidity,
`Q = 2^64`. Every √price is stored as the integer `⌊√P · Q⌋` or `⌈√P · Q⌉` (Q64.64).

### 1. Tick spacing and the price domain

**Definition.** Tick `i` is the price `P(i) = 1.0001^i`, so `√P(i) = 1.0001^{i/2}`.
Adjacent ticks differ by exactly 0.01% (1 bp) in price.

**Bins.** With spacing `s`, positions may only use ticks `k·s`. The price axis is
partitioned into bins `B_k = [P(ks), P((k+1)s))`. The relative width of every bin
is `P((k+1)s)/P(ks) − 1 = 1.0001^s − 1`, independent of `k`: the grid is
geometrically uniform, so a bin covers the same *percentage* range at any price.

**Domain bound.** `MAX_TICK = −MIN_TICK = 443 636`, so

```
√P(MAX_TICK) = exp(221 818 · ln 1.0001) = exp(22.1807…) = 2^{31.9999…}
```

Hence `√P·Q ∈ [≈2^32, ≈2^96]`, leaving 32 bits of headroom in `u128`. This is what
lets every product in §3–§4 fit in a 256-bit intermediate `(u128, u128)` and every
quotient that is a price fit back in `u128`. Price range: `P ∈ [5.4·10⁻²⁰, 1.8·10¹⁹]`.

**Computing √P(i).** Write `|i| = Σ_b β_b 2^b` (β_b ∈ {0,1}, b ≤ 18 since
`|i| < 2^19`). Then

```
1.0001^{−|i|/2} = Π_{b : β_b = 1} c_b,     c_b = 1.0001^{−2^{b−1}} < 1
```

The constants `c_b · 2^128` (Q128.128) are precomputed (identical to audited
Uniswap v3 constants, re-derived at 120-digit precision; each is the ceiling, off
by ≤ 1 ulp). The product is accumulated with `(r · c_b) >> 128`, i.e. the high
word of `full_mul`. For `i < 0` the result is shifted to Q64.64; for `i > 0` it is
inverted as `⌈2^192 / r⌉`. Relative error before the final rounding is below
`19 · 2^−128`; after it, at most one Q64.64 ulp, i.e. `≤ 1/(√P·Q) ≤ 2^−32`.

**Monotonicity** (`i < j ⇒ √P(i) < √P(j)`) is required for the tick search and is
verified *exhaustively* over all 887 273 ticks
(`sqrt_price_is_strictly_monotone_over_entire_domain`).

**Inverse.** `tick(√P) = max{ t : √P(t) ≤ √P }` by binary search on the monotone
map. The bracket is narrowed with `k = ⌊log₂ √P⌋` from the leading-zero count:
`t ∈ [2k·log_{1.0001}2, 2(k+1)·log_{1.0001}2)` with `log_{1.0001}2 ∈ (6931, 6932)`,
so ≤ 14 probes. Inside a swap the bracket is a single bitmap word (≤ 7 probes).

**Per-tick liquidity cap.** Let `N = ⌊MAX/s⌋ − ⌈MIN/s⌉ + 1` be the number of usable
ticks and `cap = ⌊(2^128 − 1)/N⌋`. The active liquidity at any price is a sum of
positions containing that price; each such position contributes to the gross
liquidity of at least one tick, so `L_active ≤ Σ_ticks gross ≤ N · cap < 2^128`.
Pool liquidity can therefore never overflow (`domain_constants` checks
`N · cap` for several spacings).

### 2. Liquidity model

Within a bin the pool behaves like a constant-product curve on *virtual* reserves
`(x_v, y_v)` with `x_v · y_v = L²`, `√P = √(y_v/x_v)`, hence

```
x_v = L / √P,     y_v = L · √P.
```

A position on `[P_a, P_b]` only needs to back the curve between its bounds: at
`P = P_b` it must hold no token0 and at `P = P_a` no token1. Subtracting those
virtual reserves gives the **real** reserves

```
x = L (1/√P − 1/√P_b),      y = L (√P − √P_a)        for P ∈ [P_a, P_b]
```

(equivalently `(x + L/√P_b)(y + L√P_a) = L²`). Clamping `P` to the range gives the
three cases implemented in `modify_position`:

| Current price | token0 | token1 |
|---|---|---|
| `P < P_a` | `L(1/√P_a − 1/√P_b)` | 0 |
| `P_a ≤ P < P_b` | `L(1/√P − 1/√P_b)` | `L(√P − √P_a)` |
| `P ≥ P_b` | 0 | `L(√P_b − √P_a)` |

Solving for `L` (`liquidity_for_amounts`): `L₀ = Δx·√P_a·√P_b/(√P_b − √P_a)`,
`L₁ = Δy/(√P_b − √P_a)`; in range both constraints bind so `L = min(L₀, L₁)`.
Both are floored, so the minted position never costs more than the budget
(`liquidity_for_amounts_never_overspends`).

**Capital efficiency.** Relative to a full-range position, the same `L` in
`[P_a, P_b]` needs a fraction `1 − (P_a/P_b)^{1/4}` of the capital (at the
geometric mid-price), e.g. a ±1% range needs ≈0.5%: a ~200× multiplier.

### 3. Exact token amounts in fixed point

With stored values `a = √P_a·Q`, `b = √P_b·Q`:

```
Δy = L(b − a) / Q                  Δx = L·Q·(b − a) / (a·b)
```

`Δy` is a single `mul_div`: exact floor/ceil.

`Δx` has a numerator of up to 128+64+96 bits. Instead of 320-bit arithmetic it is
computed **exactly** by three quotient/remainder divisions:

```
L(b−a) = t·b + r                   (0 ≤ r < b;  t < L since b−a < b)
t·Q    = T·a + r₁                  (0 ≤ r₁ < a)
r·Q    = u·b + r₂                  (0 ≤ r₂ < b;  u < Q since r < b)
```

Then

```
Δx = (t·b + r)·Q/(a·b) = t·Q/a + r·Q/(a·b)
   = T + r₁/a + (u + r₂/b)/a
   = T + (r₁ + u + r₂/b)/a.
```

Since `r₁ + u` is an integer and `0 ≤ r₂/b < 1`,
`⌊(r₁ + u + r₂/b)/a⌋ = ⌊(r₁ + u)/a⌋`. Therefore

```
⌊Δx⌋ = T + ⌊(r₁+u)/a⌋,     ⌈Δx⌉ = ⌊Δx⌋ + [ (r₁+u) mod a ≠ 0  ∨  r₂ ≠ 0 ].
```

Both are verified bit-for-bit against an arbitrary-precision reference over the
entire tick domain (`amount0_delta_is_exact`, `amount1_delta_is_exact`).

The 256/128 division itself (`div_256_by_128`) is Knuth's algorithm D with two
64-bit digits; `mul_div_rem` is verified against `BigUint` for random full-width
operands, including the overflow boundary (`mul_div_matches_bigint`).

### 4. Swap step and cross-tick execution

**Price after an input.** From `x_v = L/√P`, adding `Δx` gives
`L/√P' = L/√P + Δx`, and from `y_v = L√P`, adding `Δy` gives `√P' = √P + Δy/L`:

```
√P' = L·Q / (⌊L·Q/√P⌋ + Δx)   (token0 in, rounded up)
√P' = √P + ⌊Δy·Q/L⌋           (token1 in, rounded down)
```

*Rounding proof (token0 in).* Flooring `L·Q/√P` makes the denominator ≤ exact, so
the quotient is ≥ exact; taking the ceiling keeps it ≥ exact. A higher price
after selling token0 means *less* token1 is released. The error is < 1 unit of
token0 because the denominator is expressed in token0 units. (For `L·Q/√P ≥ 2^128`
the algebraically equal `L√P/(L + Δx√P/Q)` is used with a floored denominator,
same direction.)

*Rounding proof (token1 in).* Flooring `Δy·Q/L` gives `√P' ≤` exact, i.e. less
token0 released.

**Step (`compute_swap_step`)**, for remaining input `R` and fee `f` (pips):

1. `R' = ⌊R(10⁶ − f)/10⁶⌋`.
2. `A = ⌈amount to reach target⌉`. If `R' ≥ A` the step ends at the target
   (bin boundary or price limit), else at `√P'(R')`.
3. `in = ⌈amount to reach the actual end⌉`, `out = ⌊amount released⌋`.
4. `fee = ⌈in·f/(10⁶ − f)⌉` if the target was reached, else `R − in`.

**Invariant `in + fee ≤ R`.** Target reached: `R' ≥ in` ⇒ `R(1−φ) ≥ in` (φ = f/10⁶)
⇒ `R ≥ in + in·φ/(1−φ)` ⇒ since `R` is an integer, `R ≥ in + ⌈in·φ/(1−φ)⌉`.
Not reached: by the rounding proofs, `√P'(R')` moves the price *at most* as far
as `R'` pays for, so `in ≤ R' ≤ R` and `fee = R − in ≥ 0`. An overflowing `A` is
treated as "not reachable", which is sound since `R' < 2^128`. The swap loop
therefore decrements `remaining` with no underflow — and this is also asserted
by the fuzzer (`swap_step_invariants`) together with `in ≥ ⌈exact cost⌉` and
`out ≤ ⌊exact value⌋`.

**Cross-tick loop.** Each iteration finds the next initialized tick in the swap
direction within one 128-bit bitmap word (or stops at the word boundary),
executes one step at constant `L`, and if the boundary is reached, *crosses* it:
`L ← L ± liquidity_net(tick)`. `liquidity_net` is `+ΔL` at a lower bound and `−ΔL`
at an upper bound, so by induction the active liquidity always equals the sum of
positions containing the current price. The loop ends when input is exhausted or
the price limit is hit; unspent input is never charged. Each iteration either
consumes all input or advances to a strictly farther tick/word, so it terminates.

Tick convention: after crossing tick `t` downward the current tick is `t − 1`
(price at the lower edge belongs to the lower bin); after stopping inside a bin
the tick is recomputed by the bracketed binary search.

### 5. Fee distribution to active ranges only

Fee growth `g` is fees per unit of *active* liquidity (Q64.64), updated each step:
`g += ⌊fee·Q / L_active⌋`. Each initialized tick stores `o(t)`, the growth on the
side of `t` *opposite* the current price. On initialization `o(t) = g` if
`t ≤ tick_current` (all history attributed below), else 0; on each crossing
`o(t) ← g − o(t)`.

**Claim.** `o(t)` equals the growth accrued on the side of `t` not containing the
price. *Proof by induction on crossings:* true at initialization by the
convention (any consistent convention works because only differences of the
inside value are ever used). A crossing flips which side is "opposite"; growth on
the new opposite side equals total minus the old opposite side, i.e. `g − o(t)`. ∎

Then growth below `t_l`, above `t_u` and inside are

```
below = t_c ≥ t_l ? o(t_l) : g − o(t_l)
above = t_c < t_u ? o(t_u) : g − o(t_u)
inside = g − below − above
```

and a position earns `L_pos · (inside_now − inside_last) / Q`. Growth only
increases `inside` while `t_l ≤ t_c < t_u`, i.e. while the swap is executing
within the position's range, so **only LPs whose range is actively crossed earn
fees**, pro rata to their `L` (`fees_accrue_only_to_ranges_that_are_crossed`,
`fees_split_pro_rata_between_overlapping_positions`).

**Modular arithmetic.** All growth values are taken mod 2^128 with explicit
`wrapping_*` operations. Every consumed quantity is a *difference* of growths that
is truly in `[0, 2^128)`, and subtraction mod 2^128 of values congruent to the
true ones yields the true difference exactly. This is intentional modular
arithmetic, not an underflow; every other operation in the contract is `checked_*`
and maps to `Error::ArithmeticOverflow`.

### 6. Precision bound

After all LPs exit, the pool holds only rounding dust, all in the pool's favour:

* mint rounds amounts up and burn rounds down: ≤ 2 units per token per position;
* fee growth floor: `< L_active/Q` per step, i.e. < 1 unit while `L < 2^64`;
* owed-fee floor at settlement: < 1 unit per position.

`fuzz_multi_tick_swaps_conserve_value` checks, for random spacing, fee tier,
LP ranges and sequences of orders up to `2^70` units that sweep up to 20 000
ticks: trader balances change by exactly the reported amounts; selling the output
back never returns more than was paid; every LP can burn and collect in full; and
the residual per token is `≤ 4·positions + swaps·(2·positions + 4)`.

### 7. Execution cost

All math is native `u128` (no host `U256` objects, no floats). `sqrt_price_at_tick`
is ≤ 19 multiplications; a step is ~10 `mul_div`s; storage is touched once per
bitmap word and once per crossed tick (the word is cached across steps). Under
real WASM metering, a single order crossing 40 initialized bins costs
~17 M CPU instructions, against a 100 M network limit
(`wasm_multi_tick_swap_fits_budget`).
