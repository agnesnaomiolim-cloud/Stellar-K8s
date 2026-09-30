# cl-amm

Concentrated Liquidity AMM (CLAMM) — the contract surface for
[Stellar-K8s] issue #285.

## Scope

- **Tick-based pricing**: a logarithmic tick grid, each tick a bounded
  price bin (`sqrt_price` given by `2 ** (i / SPREAD)`).
- **Cross-tick swapping**: a swap that spans several ticks walks the
  sorted tick grid, executing a constant-product single-tick swap at
  every crossed tick.
- **Dynamic fee collection**: fees accrue per tick proportionally to
  liquidity intersected, with exact integer distribution.
- **WASM-safe fixed-point math**: all values are `u64` in the `1e7`
  fixed-point domain, with fuel/overflow guards so zero arithmetic
  underflow occurs in a WASM execution sandbox.

## Files

- `src/lib.rs` — crate root; re-exports `Tick`, `TickSpacing`, `Pool`,
  `Fee`, `SwapOutcome`, `SwapError`.
- `src/ticks.rs` — the tick grid, spacing, and fixed-point constants.
- `src/pool.rs` — the pool and cross-tick swap engine.
- `tests/clamm_fuzz.rs` — proptest + direct edge-case fuzz tests for
  massive multi-tick market orders.

## Build

```sh
# Build for WASM32 (the contract runtime).
cargo build --release --target wasm32-unknown-unknown

# Run the fuzz tests.
cargo test --test clamm_fuzz
```

The crate is a workspace member of the root `Cargo.toml` under the
`contracts/cl-amm` directory.
