// SPDX-License-Identifier: Apache-2.0
use soroban_sdk::{Env, Address, map};
use soroban_sdk::stl::{map, try_from_val, vec};

pub struct LockManager<'a> {
    env: &'a Env,
    locks: map::Map<Address, bool>,
}

impl<'a> LockManager<'a> {
    pub fn new(env: &'a Env) -> Self {
        let locks = map::Map::new(env);
        LockManager { env, locks }
    }

    pub fn acquire_lock(&mut self, contract: &Address) {
        self.locks.set(contract, true);
    }

    pub fn release_all(&mut self, env: &Env) {
        self.locks.clear();
    }

    pub fn is_locked(&self, contract: &Address) -> bool {
        self.locks.get(contract).unwrap_or(false)
    }
}