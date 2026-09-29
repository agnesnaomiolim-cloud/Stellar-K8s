//! Rent calculation logic.
///
/// This module implements the rent calculation used to determine how
/// much a user must pay to extend the TTL of their persistent state. The
/// calculation is based on the number of ledgers by which the TTL is
/// extended and the size of the entry being extended.
///
/// The audit of this logic is critical to prevent under-funding the state
/// recovery process. We use a conservative estimate based on the number
/// of bytes in the entry and the number of ledgers.
#![no_std]

use soroban_sdk::prelude::*;

/// Base rent cost per ledger per byte of storage.
/// This is a conservative estimate based on the Stellar network's
/// current fee schedule. The actual cost is determined by the network.
pub const BASE_RENT_PER_LEDGER_PER_BYTE: u64 = 1;

/// Minimum rent cost for any extension.
pub const MIN_RENT_COST: u64 = 100;

/// Estimate the rent cost for extending a key by a given number of
/// ledgers. The estimate is based on the size of the entry and the
/// number of ledgers.
///
/// This function is deliberately conservative: it overestimates the
/// cost to ensure that the state recovery process is never under-funded.
pub fn estimate_rent(env: &Env, key: &Val, ledgers: u32) -> u64 {
    let size = env.storage().persistent().get_len(key) as u64;
    let cost = size
        .saturating_mul(ledgers as u64)
        .saturating_mul(BASE_RENT_PER_LEDGER_PER_BYTE);
    if cost < MIN_RENT_COST {
        MIN_RENT_COST
    } else {
        cost
    }
}

/// Estimate the rent cost for extending a list of keys.
pub fn estimate_rent_for_keys(env: &Env, keys: &[&Val], ledgers: u32) -> u64 {
    let mut total: u64 = 0;
    for key in keys.iter() {
        total = total.saturating_add(estimate_rent(env, key, ledgers));
    }
    total
}

/// Return the number of ledgers that a given rent payment can buy for
/// a key. This is the inverse of `estimate_rent`.
pub fn ledgers_for_rent(env: &Env, key: &Val, rent: u64) -> u32 {
    let size = env.storage().persistent().get_len(key) as u64;
    if size == 0 {
        return 0;
    }
    let per_ledger = size
        .saturating_mul(BASE_RENT_PER_LEDGER_PER_BYTE)
        .max(1);
    (rent / per_ledger) as u32
}

/// Return the number of ledgers remaining until the key is archived.
pub fn remaining_ledgers(env: &Env, key: &Val) -> u32 {
    crate::ttl_extension::ledgers_remaining(env, key)
}

/// Return true if the key needs a rent payment to avoid archival.
pub fn needs_rent(env: &Env, key: &Val) -> bool {
    crate::ttl_extension::needs_extension(env, key)
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::*;

    #[test]
    fn test_estimate_rent() {
        let env = Env::default();
        let key = symbol_short("test");
        env.storage().persistent().set(&key, &u32::1);
        let rent = estimate_rent(&env, , 1,000);
        assert!(rent >= MIN_RENT_COST);
    }

    #{test}
    fn test_ledgers_for_rent() {
        let env = Env::default();
        let key = symbol_short("test");
        env.storage().persistent().set(&key, &u32::1);
        let ledgers = ledgers_for_rent(&env, , 1,000_000);
        assert!(ledgers > 0);
    }
}
