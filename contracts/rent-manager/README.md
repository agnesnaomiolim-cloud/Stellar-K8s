# Rent Manager

State Archival & Rent Restoration Manager for Stellar Soroban contracts.

## Overview

Stellar's Protocol 20 introduced state archival, where inactive persistent data
(like old token balances) is evicted from the live network once its TTL hits
zero. This contract acts as an automated rent manager, allowing users or
automated keepers to seamlessly pay storage rent and restore archived data
back to the live ledger without disrupting the overarching application logic.

## Features

- **TTL Tracking**: Tracks the TTL of critical protocol state variables.
- **Automatic TTL Extension**: Extends TTL at the beginning of every
  user-facing interaction to keep highly active accounts alive.
- **Restoration Proxy**: Allows users to submit a rent-payment transaction
  to recover their personal state if their data was archived during
  prolonged inactivity.
- **Strategic Extension**: Extends only the specific keys accessed within
  a function to minimize multi-dimensional fee inflation.

## Modules

- `contracts/rent-manager/src/lib.rs`: Main contract implementation.
- `contracts/rent-manager/src/ttl_extension.rs`: TTL extension helpers.
- `contracts/rent-manager/src/restore.rs`: Restoration proxy for archived
  user state.
- `contracts/rent-manager/src/rent.rs`: Rent calculation logic.

## Building

```bash
cargo build --package rent-manager
```

## Testing

```bash
cargo test --package rent-manager
```

The test suite includes a validation that fast-forwards a local testnet by
10,000 ledgers to force an archival event, then executes the restore
function to prove the user's funds are safely recovered.

## Rent Calculation Audit

The rent calculation in `src/rent.rs` is deliberately conservative:

- The base rate is computed as `size * ledgers * BASE_RENT_PER_LEDGER_PER_BYTE`.
- A minimum cost of `MIN_RENT_COST` is enforced to avoid under-funding.
- All additions use `saturating_add` to prevent overflow.
- The estimate overestimates the cost to ensure the state recovery process
  is never under-funded.

## License

Apache-2.0
