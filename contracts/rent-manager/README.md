# Rent Manager Contract

Automated storage rent manager for Stellar Protocol 20 state archival. This contract tracks the TTL of critical persistent state, extends it during active use, and restores archived user data after idle periods.

## Overview

Stellar's Protocol 20 introduced state archival. Persistent entries (token balances, user deposits, total supply) are evicted from the live ledger once their TTL reaches zero. When an entry is evicted, its data is still recoverable by submitting a restore operation that pays the required rent. This contract abstracts that process for applications and their users.

The contract is organized into two modules:

- **`contracts/rent-manager/src/lib.rs`** — public entry points, storage layout, and the restoration proxy.
- **`contracts/rent-manager/src/ttl_extension.rs`** — strategic TVL extension loop used at the beginning of every public interaction.

## Key Features

- **TTL tracking** for critical protocol state (total supply, user deposits, admin configuration).
- **Strategic TVL extension** that only touches the keys actually accessed by the current call, minimizing multi-dimensional fee inflation.
- **Restoration proxy** that lets a user submit a rent payment to recover their archived personal state.
- **Auditable rent calculation** that derives the minimum required fee from the number of ledgers and the current fee configuration.

## Storage Layout

All critical entries use persistent storage with explicit TTL management:

| Key                 | Type     | Description                                    |
|---------------------|----------|----------------------------------------------------|
| `TotalSupply`      | `asset`   | Aggregate deposited amount                          |
| `UserDeposit(addr)` | `asset`   | Per-user deposit balance                           |
| `Admin`             | `Address` | Admin authorized to configure TTL policy             |
| `TtlPolicy`         | `struct`  | Minimum TTL threshold and extension amount          |

## TTL Extension Strategy

The extension logic in `ttl_extension.rs` is deliberately narrow. Instead of extending every key in the contract, it receives the exact set of keys the caller is about to read or write and extends only those. This avoids paying for TTL on unrelated entries and keeps fees proportional to the work being done.

```rust
pub fn extend_keys(env: &Env, keys: &[LedgerKey], threshold: u32, extend_to: u32) {
    for key in keys {
        let ttl = env.storage().persistent().get_ttl(key);
        if ttl < threshold {
            env.storage().persistent().extend_ttl(key, extend_to);
        }
    }
}
```

Every public entry point in `lib.rs` calls `extend_keys` with the specific keys it will touch before doing any other work. The admin configures the threshold and target TTL once via `set_ttl_policy`.

## Restoration Proxy

When a user's deposit entry has been evicted, a regular read will fail. The restoration proxy exposes `restore_user` which:

1. Accepts the user address and a rent payment amount.
2. Calls `env.storage().persistent().restore(key, min_ledger_to_live)` for the user's deposit key.
3. Extends the restored key's TTL so it does not immediately expire again.
4. Returns the recovered balance to the caller.

The function is idempotent: if the entry is already live, it simply extends the TTL and returns the current balance without charging a restore fee.

## Rent Calculation Audit

The restore fee must be computed from the actual number of ledgers the entry will be extended and the current fee configuration. The audit checklist is implemented in the contract as follows:

1. **Read the current ledger** via `env.ledger().sequence()`.
2. **Compute the target live ledger** as `current_ledger + extension_ledges`.
3. **Derive the minimum rent** from the number of ledgers and the current fee configuration returned by the host.
4. **Reject under-funded restores** with `Error::RentUnderfunded` when the submitted amount is less than the derived minimum.
5. **Refund the excess** back to the caller after the restore succeeds.

This prevents the classic failure mode where a restore is attempted with too few fees, leaving the entry evicted and the user's funds unrecoverable until another payment is made.

## Building and Testing

```bash
cargo build -p rent-manager
cargo test -p rent-manager
```

The test suite includes an archival simulation that fast-forwards the local testnet by 10,000 ledgers, forcing the user deposit entry to be evicted, and then executes `restore_user` to prove the funds are safely recovered.

## Security Notes

- Only the admin can change the TTL policy.
- Restoration is permissionless for the owner of the address being restored; any address may pay to restore any other address.
- TTL extension is bounded by the configured maximum so a single call cannot consume unbounded rent.
- All arithmetic on balances uses checked operations to avoid overflow.
