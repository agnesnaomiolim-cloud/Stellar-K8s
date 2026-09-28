# On-Chain Options Vault (Call/Put)

A Soroban sub-contract that lets writers escrow collateral and mint
**standardized, fungible European option tokens**, then settles every series
deterministically against an oracle after its expiry timestamp.

* `src/lib.rs` — the `OptionsVault` contract: series registry, collateral
  escrow, fungible option-token ledger, oracle-driven settlement and claims.
* `src/settlement.rs` — pure, `Env`-free payoff/collateral/oracle math.
* `src/test.rs` — end-to-end tests over a real Soroban host with a mock oracle
  and real Stellar Asset Contract tokens.

## Model

A **series** is identified by `(kind, underlying, collateral, strike, expiry)`.
Every option token inside a series is fungible — any two "XLM $1.00 call
expiring at T" tokens are interchangeable.

| Kind   | Escrowed collateral per contract | Payoff when in-the-money                       |
| ------ | -------------------------------- | ---------------------------------------------- |
| `Call` | `CONTRACT_SIZE` of the underlying | `(spot - strike) * CONTRACT_SIZE / spot` underlying |
| `Put`  | `strike` of the quote asset       | `strike - spot` quote                          |

`strike` and the oracle price are both scaled by `PRICE_SCALE = 10^7` (Stellar's
7-decimal precision) and expressed as collateral units per **whole** underlying
unit. One contract covers `CONTRACT_SIZE = 10^7` raw underlying units.

Settlement splits the series escrow into two pools that always sum to exactly
the collateral held:

* the **payoff pool** is redeemed by option holders, pro-rata by token balance;
* the **residual pool** is claimed by writers, pro-rata by escrowed collateral.

A call written against the underlying and a put written against the quote asset
are therefore both *fully* collateralized. Out-of-the-money and exactly
at-the-money series pay holders nothing and return the entire escrow to writers.

## Lifecycle

```text
create_series ──> write_option ──> transfer (peer-to-peer)
                      │
                      └── before expiry
expiry ──> settle (oracle)  ──> redeem (holders) / claim_collateral (writers)
```

1. `create_series(creator, key)` registers a series. Calls must escrow the
   underlying itself; puts must escrow a distinct quote asset.
2. `write_option(writer, key, amount)` transfers the required collateral into
   the vault and mints `amount` option tokens to the writer. Rejected at or
   after expiry.
3. `transfer(from, to, key, amount)` moves fungible option tokens.
4. `settle(key)` may be called by anyone at or after expiry. It queries the
   oracle once and freezes the settlement price and both pools.
5. `redeem(holder, key)` pays holders their share of the payoff pool;
   `claim_collateral(writer, key)` returns writers their share of the residual.

## Oracle safety (missing / delayed feeds)

Settlement requires an observation that was

* published **at or after** the series expiry (`OraclePricePredatesExpiry`), and
* no older than `max_staleness` seconds (`OraclePriceStale`), and
* not stamped in the future (`OraclePriceFromFuture`).

If the feed has no data at all, `settle` returns `OraclePriceUnavailable`. In
every one of these cases **nothing is mutated**, so the call can simply be
retried once the feed catches up. `settlement_ready(key)` lets keepers poll for
the exact moment the vault will accept settlement instead of retrying blindly.

The admin can repoint the feed (`set_oracle`) or tighten the freshness window
(`set_max_staleness`).

## Build and test

This crate is a standalone Soroban workspace (like `contracts/staking-vault`):

```console
cd contracts/options-vault
cargo test
```
