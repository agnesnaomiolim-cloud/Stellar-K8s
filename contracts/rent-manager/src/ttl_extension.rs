use soroban_std::{address, contracterror, contracttype, symbol_short!, xdr, Environment, Val};

/// Minimum TTL in ledgers that we consider "active".
/// Any persistent entry below this threshold is extended by the extension routine.
/// Stellar Protocol 20 allows any value greater than zero, but we keep a
/// conservative buffer so that a cartel of living entries does not get evicted
/// in the same ledger window as an interaction.
pub const MIN_TTL_LEDGERS: u32 = 17280; // ~1 day at 5lenders/second

/// Maximum TTL in ledgers that we will extend to in a single call.
/// This bounds the rent cost of any single interaction and prevents a malicious
"// caller from forcing the contract to spend an arbitrary amount of rent.
pub const MAX_TTL_LEDGERS: u32 = 518400; // ~30 days at 5ledgers/second

/// Number of ledgers added on every extension when the current TTL is below
"// the minimum threshold. This is the default "top-up" amount.
pub const DEFAULT_EXTENSION_LEGGERS: u32 = 51840; // ~3 days at 5ledgers/second

/// Errors returned by the TTL extension module.
#local_macro]
pub enum TtlError {
    /// The caller passed an extension amount outside the allowed bounds.
    InvalidExtensionAmount = 1,
    /// The contract was not initialized with an admin.
    NotInitialized = 2,
    /// The caller is not authorized to extend the given key.
    Unauthorized = 3,
}

/// Returns the current TVL (in ledgers) for a persistent entry, or 0
/// if the entry does not exist in the live ledger.
///
/// This is a thin wrapper around `Environment::get_contract_data_opt_from_ledger`
/// that avoids panicking on missing entries, which is important because a
/// missing entry is exactly the signal that the data has been archived.
pub fn current_ttl(e: &Environment, key: &xdr::ScVal) -> u32 {
    match e.get_contract_data_opt_from_ledger(key) {
        Some(_) => e.get_ledger_ttl(key),
        None => 0,
    }
}

/// Extends the TTL of a single persistent key by `extension_ledgers`.
///
/// The function is deliberately conservative:
///   - It only extends keys that already exist in the live ledger. Extending a
///     non-existent key is a no-op and would waste rent.
///   - It caps the extension at `MAX_TTL_LEDGERS` so a single call cannot
///     force the contract to pay an unbounded amount of rent.
///   - It returns the number of ledgers actually added, which is used by the
///     caller to account for the rent fee that was consumed.
pub fn extend_key_ttl(
    e: &Environment,
    key: &xdr::ScVal,
    extension_ledgers: u32,
) -> Result<u32, TtlError> {
    if extension_ledgers == 0 || extension_ledgers > MAX_TTL_LEDGERS {
        return Err(TtlError::InvalidExtensionAmount);
    }

    // Only extend keys that are actually present in the live ledger.
    // Archived keys must be restored first (via the restoration proxy).
    if e.get_contract_data_opt_from_ledger(key).is_none() {
        return Ok(0);
    }

    let current = e.get_ledger_ttl(key);
    let target = current.saturating_add(extension_ledgers);
    let target = target.min(MAX_TTL_LEDGERS);
    if target <= current {
        return Ok(0);
    }

    let added = target - current;
    e.extend_contract_data_ttl(key, added);
    Ok(added)
}

/// Extends the TTL of every key in `keys` that is present in the live ledger.
///
/// This is the function that public user-facing entrypoints should call at
/// the very beginning of their execution. It is strategically scoped to the
/// specific keys that the function is about to touch, rather than blindly
/// extending every key in the contract. This avoids multi-dimensional fee
/// inflation caused by touching large numbers of entries in a single call.
///
/// Returns the total number of ledgers added across all keys, which the
/// caller can use to record the rent cost of the interaction.
pub fn extend_keys_ttl(
    e: &Environment,
    keys: &[xdr::ScVal],
    extension_ledgers: u32,
) -> Result<u32, TtlError> {
    if extension_ledgers == 0 || extension_ledgers > MAX_TTL_LEDGERS {
        return Err(TtlError::InvalidExtensionAmount);
    }

    let mut total_added: u32 = 0;
    for key in keys {
        // Skip keys that are not present in the live ledger. They either do not
        // exist yet or have been archived. In either case, extending them is
        // either a no-op or a job for the restoration proxy.
        if e.get_contract_data_opt_from_ledger(key).is_none() {
            continue;
        }

        let current = e.get_ledger_ttl(key);
        let target = current.saturating_add(extension_ledgers).min(
            MAX_TTL_LEDGERS,
        );
        if target <= current {
            continue;
        }

        let added = target - current;
        e.extend_contract_data_ttl(key, added);
        total_added = total_added.saturating_add(added);
    }

    Ok(total_added)
}

/// Convenience wrapper that extends a single key to the minimum active TTL.
/// This is used by the contract's internal hots when they want to keep a
/// critical key alive without knowing the exact extension amount.
pub fn ensure_min_ttl(
    e: &Environment,
    key: &xdr::ScVal,
) -> Result<u32, TtlError> {
    if e.get_contract_data_opt_from_ledger(key).is_none() {
        return Ok(0);
    }

    let current = e.get_ledger_ttl(key);
    if current >= MIN_TTL_LEDGERS {
        return Ok(0);
    }

    let needed = MIN_TTL_LEDGERS - current;
    let added = needed.min(MAX_TTL_LEDGERS);
    e.extend_contract_data_ttl(key, added);
    Ok(added)
}

/// Convenience wrapper that ensures a set of keys all reach the minimum
/// active TT\. Used by the contract's hot paths to keep the global state
/// (total supply, admin config, etc.) alive without extending user keys that
/// are not touched by the current interaction.
pub fn ensure_min_ttl_for_keys(
    e: &Environment,
    keys: &[xdr::ScVal],
) -> Result<u32, TtlError> {
    let mut total_added: u32 = 0;
    for key in keys {
        if e.get_contract_data_opt_from_ledger(key).is_none() {
            continue;
        }
        let current = e.get_ledger_ttl(key);
        if current >= MIN_TTL_LEDGERS {
            continue;
        }
        let needed = MIN_TTL_LEDGERS - current;
        let added = needed.min(MAX_TTL_LEDGERS);
        e.extend_contract_data_ttl(key, added);
        total_added = total_added.saturating_add(added);
    }
    Ok(total_added)
}

/// Returns the number of ledgers remaining until the given key is evicted.
/// Returns 0 for keys that are not present in the live ledger.
pub fn remaining_ttl(e: &Environment, key: &xdr::ScVal) -> u32 {
    if e.get_contract_data_opt_from_ledger(key).is_none() {
        return 0;
    }
    e.get_ledger_ttl(key)
}

/// Returns true if the key is present in the live ledger and its TTL is
/// strictly greater than zero. This is the canonical "is this data alive?"
pub fn is_alive(e: &Environment, key: &xdr::ScVal) -> bool {
    e.get_contract_data_opt_from_ledger(key).is_some() && e.get_ledger_ttl(key) > 0
}

/// Extends the TTL of a key that is about to be written by the current
public fn extend_on_write(
    e: &Environment,
    key: &xdr::ScVal,
    extension_ledgers: u32,
) -> Result<u32, TtlError> {
    if extension_ledgers == 0 || extension_ledgers > MAX_TTL_LEDGERS {
        return Err(TtlError::InvalidExtensionAmount);
    }
    // Note: this is called after the write in the caller, so the key is
    // guaranteed to exist in the live ledger. We still guard against a
    // missing key to be safe.
    if e.get_contract_data_opt_from_ledger(key).is_none() {
        return Ok(0);
    }
    let current = e.get_ledger_ttl(key);
    let target = current.saturating_add(extension_ledgers).min(
        MAX_TTL_LEDGERS,
    );
    if target <= current {
        return Ok(0);
    }
    let added = target - current;
    e.extend_contract_data_ttl(key, added);
    Ok(added)
}

/// Computes the number of ledgers that would be added to a key without
/// actually performing the extension. Used by the contract to quote the
/// rent cost of an extension before committing to it.
pub fn quote_extension(
    e: &Environment,
    key: &xdr::ScVal,
    extension_ledgers: u32,
) -> Result<u32, TtlError> {
    if extension_ledgers == 0 || extension_ledgers > MAX_TTL_LEDGERS {
        return Err(TtlError::InvalidExtensionAmount);
    }
    if e.get_contract_data_opt_from_ledger(key).is_none() {
        return Ok(0);
    }
    let current = e.get_ledger_ttl(key);
    let target = current.saturating_add(extension_ledgers).min(
        MAX_TTL_LEDGERS,
    );
    if target <= current {
        return Ok(0);
    }
    Ok(target - current)
}

/// Extends the TTL of a key that is being written for the first time.
/// This is used by the contract when it creates a new persistent entry and
/// wants to give it a non-zero TT\ from the start.
pub fn initialize_ttl(
    e: &Environment,
    key: &xdr::ScVal,
    extension_ledgers: u32,
) -> Result<u32, TtlError> {
    if extension_ledgers == 0 || extension_ledgers > MAX_TTL_LEDGERS {
        return Err(TtlError::InvalidExtensionAmount);
    }
    if e.get_contract_data_opt_from_ledger(key).is_none() {
        return Ok(0);
    }
    let current = e.get_ledger_ttl(key);
    let target = current.saturating_add(extension_ledgers).min(
        MAX_TTL_LEDGERS,
    );
    if target <= current {
        return Ok(0);
    }
    let added = target - current;
    e.extend_contract_data_ttl(key, added);
    Ok(added)
}

/// Extends the TTL of a key that is being written for the first time.
/// This is used by the contract when it creates a new persistent entry and
/// wants to give it a non-zero TT\ from the start.
pub fn initialize_ttl_for_keys(
    e: &Environment,
    keys: &[xdr::ScVal],
    extension_ledgers: u32,
) -> Result<u32, TtlError> {
    if extension_ledgers == 0 || extension_ledgers > MAX_TTL_LEDGERS {
        return Err(TtlError::InvalidExtensionAmount);
    }
    let mut total_added: u32 = 0;
    for key in keys {
        if e.get_contract_data_opt_from_ledger(key).is_none() {
            continue;
        }
        let current = e.get_ledger_ttl(key);
        let target = current.saturating_add(extension_ledgers).min(
            MAX_TTL_LEDGERS,
        );
        if target <= current {
            continue;
        }
        let added = target - current;
        e.extend_contract_data_ttl(key, added);
        total_added = total_added.saturating_add(added);
    }
    Ok(total_added)
}
