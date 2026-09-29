# Index Basket & Factory

Soroban contracts for index funds on Stellar. Each basket is a SEP-41 token backed in full by a fixed set of underlying tokens (for example 50% XLM, 30% USDC, 20% sBTC).

| Crate | Path | Role |
|---|---|---|
| `index-basket` | `src/` | The basket token: issuance, redemption, and rebalancing |
| `index-basket-factory` | `factory/` | Deploys baskets from an uploaded WASM hash and keeps a registry of them |

## Lifecycle

1. **Create.** `factory.create_basket(creator, salt, params)` deploys a basket. The constructor checks the components (2–10 unique tokens, weights > 0 that sum to 10 000 bps, token decimals ≤ 18). It then derives each component's `units`, the base units that back one whole basket token, from `initial_nav`, the weights and the oracle's launch prices. If the constructor rejects the parameters, the whole deployment reverts. The deployed address is `sha256(creator ‖ salt)`, so another account cannot take a creator's address by front-running the same salt.
2. **Issue.** `issue(to, amount, max_amounts_in)` takes a deposit of every component in the basket's current ratio and mints `amount` basket tokens:
   - While the supply is zero, the ratio is set by the stored `units`.
   - After that, it is set by the live reserves.
   - Every deposit rounds **up**.
3. **Redeem.** `redeem(from, amount, min_amounts_out)` burns basket tokens and pays out the holder's pro-rata share of every component, rounded **down**.
4. **Rebalance.** `rebalance(arb, token_in, token_out, amount_in, min_amount_out)` is for authorised arbitrageurs. When price moves pull the value weights away from their targets, an arbitrageur deposits an underweight component and receives an overweight one. The exchange uses oracle prices plus a premium, `incentive_bps` (capped at 5%). A trade must meet all of these conditions:
   - `token_in` is under its target weight and `token_out` is over its target;
   - at least one of the two is outside the `tolerance_bps` band;
   - after the trade, neither weight is past its target by more than the tolerance.

   After each trade the basket recomputes both components' `units` from reserves.

## Safety properties

- **Always fully collateralised.** Rounding always favours the basket. A property test runs random issue, redeem and transfer sequences over tokens with 0, 2, 6, 7, 8, 12 and 18 decimals. It asserts that the total amount holders could redeem never exceeds reserves, and that issuing then immediately redeeming never returns more than was deposited.
- **Atomic multi-asset settlement.** Each redemption pays out every component inside one contract invocation, and the contract updates its state before making any transfer. If one transfer fails (for example a frozen asset), the whole call reverts, so a redemption never completes for only some assets. Issuance and both legs of a rebalance work the same way. Tests cover this by freezing a component partway through a transfer loop.
- **Donation-proof accounting.** Reserves are tracked internally. Tokens sent straight to the contract do not change issuance ratios.
- **Overflow-safe math.** Products that don't fit in `i128` are computed with the host's 256-bit integers. This lets 18-decimal assets with balances above 10²⁸ rebalance correctly.
- **Oracle guards.** Rebalancing reads a [SEP-40](https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0040.md) feed such as Reflector, via `lastprice(Asset::Stellar(token))`. It rejects missing prices, non-positive prices, and prices older than `max_price_age`. Issuance and redemption are in-kind and never read the oracle.

## Trust model

- The **basket admin** (who is also the creator) decides which addresses can rebalance.
- **Arbitrageurs** can only make trades that move weights toward their targets. They are paid at most `incentive_bps` on the value they move.
- The **oracle** is trusted for prices. If it is manipulated, an arbitrageur can extract value up to the drift band, so use a reputable feed and a short `max_price_age`.
- `burn` through the SEP-41 interface forfeits the underlying assets to the remaining holders. Use `redeem` to withdraw them.

## Build & test

The factory tests deploy the compiled basket WASM, so build it first:

```sh
make test      # builds both WASMs (wasm32v1-none), then runs all tests
make clippy
```

Some tests worth reading:

- `five_asset_basket_restores_20pct_weights_after_price_drift` (`src/test.rs`)
- `factory_basket_rebalances_back_to_20pct` (`factory/src/test.rs`), which runs end to end on the deployed WASM
- `rebalance_converts_across_decimal_offsets` (a 6-decimal token against an 18-decimal token)
- `prop_always_fully_collateralised`
