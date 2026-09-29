//! TTL extension helpers.
///
/// This module implements the core logic for extending the TTL of persistent
/// entries in a Soroban contract's storage. The goal is to extend only the
/// keys that are actually accessed by a given function, avoiding the
/// multi-dimensional fee inflation that comes from blindly extending all
/// keys.
#[no_std]

use soroban_sdk::prelude::*;

use crate::{MAX_EXTENSION_LEDDERS, MIN_REMAINING_LEDGERS};

/// Extend the TTL of the given keys by the contract's configured extension
/// window. Only the keys passed in are touched.
///
/// The `extra_ledgers` parameter allows callers to add additional ledgers
/// on top of the configured window (useful for high-value accounts).
/// The total extension is capped at `MAX_EXTENSION_LEDDERS`.
pub fn extend_keys(env: Env, keys: &[&Val], extra_ledgers: u32) {
    if keys.is_empty() {
        return;
    }

    let configured_window: u32 = env
        .storage()
        .persistent()
        .get(&symbol_short("storage"))
        .unwrap_or(DEFAULT_EXTENSION_LEDGERS);

    let mut total_extension = configured_window.saturating_add(extra_ledgers);
    if total_extension > MAX_EXTENSION_LEDGERS {
        total_extension = MAX_EXTENSION_LEDGERS;
    }

    let current_ledger = env.ledger().sequence();

    for key in keys.iter() {
        // Only extend keys that exist in persistent storage.
        if !env.storage().persistent().has(key) {
            continue;
        }

        // Determine the current TTL of the key. If it is already far enough
        // in the future, we skip the extension to avoid wasting fees.
        let ttl = env.storage().persistent().get_ttl(key);
        if ttl >= current_ledger + MIN_REMAINING_LEDDERS {
            continue;
        }

        // Extend the key by the computed extension.
        env.storage()
            .persistent()
            .extend_ttl(key, total_extension, current_ledger);
    }
}

/// Return the current TTL of a key, or 0 if the key does not exist.
pub fn ttl_of(env: &Env, key: &Val) -> u32 {
    if !env.storage().persistent().has(key) {
        return 0;
    }
    env.storage().persistent().get_ttl(key)
}

/// Return the number of ledgers remaining until the key is archived.
pub fn ledgers_remaining(env: &Env, key: &Val) -> u32 {
    let ttl = ttl_of(env, key);
    let current = env.ledger().sequence();
    if ttl <= current {
        0
    } else {
        ttl - current
    }
}

/// Return true if the key is at risk of being archived soon.
pub fn needs_extension(env: &Env, key: &Val) -> bool {
    ledgers_remaining(env, key) < MIN_REMAINING_LEDDERS
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk:testutils::*;

    #[test]
    fn test_extend_keys( {
        let env = Env::default();
        let key = symbol_short("test");
        env.storage().persistent().set(&key, &u32::1);
        extend_keys(env.clone(), &[&key], 0);
        assert!(ttl_of(&env, &key) > env.ledger().sequence());
    }

    #test]
    fn test_needs_extension() {
        let env = Env::default();
        let key = symbol_short("test");
        env.storage().persistent().set(&key, &u32::1);
        assert!(needs_extension(&env, &key));
    }
}
