//! Test doubles: a token with arbitrary decimals and a SEP-40 price oracle.
//! Compiled only for tests or with the `testutils` feature.

pub use mock_oracle::{MockOracle, MockOracleClient};
pub use mock_token::{MockToken, MockTokenClient};

// Separate modules: contracts in one module cannot share function names.
mod mock_token {
    use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

    #[contracttype]
    enum TokenKey {
        Decimals,
        Frozen,
        Balance(Address),
    }

    /// Minimal token with configurable precision. `set_frozen(true)` makes every
    /// transfer fail, to exercise atomicity of multi-asset flows.
    #[contract]
    pub struct MockToken;

    #[contractimpl]
    impl MockToken {
        pub fn __constructor(env: Env, decimals: u32) {
            env.storage().instance().set(&TokenKey::Decimals, &decimals);
        }

        pub fn mint(env: Env, to: Address, amount: i128) {
            let b = Self::balance(env.clone(), to.clone());
            env.storage()
                .persistent()
                .set(&TokenKey::Balance(to), &(b + amount));
        }

        pub fn set_frozen(env: Env, frozen: bool) {
            env.storage().instance().set(&TokenKey::Frozen, &frozen);
        }

        pub fn balance(env: Env, id: Address) -> i128 {
            env.storage()
                .persistent()
                .get(&TokenKey::Balance(id))
                .unwrap_or(0)
        }

        pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
            from.require_auth();
            let frozen: bool = env
                .storage()
                .instance()
                .get(&TokenKey::Frozen)
                .unwrap_or(false);
            assert!(!frozen, "token frozen");
            assert!(amount >= 0, "negative amount");
            let fb = Self::balance(env.clone(), from.clone());
            assert!(fb >= amount, "insufficient balance");
            env.storage()
                .persistent()
                .set(&TokenKey::Balance(from), &(fb - amount));
            let tb = Self::balance(env.clone(), to.clone());
            env.storage()
                .persistent()
                .set(&TokenKey::Balance(to), &(tb + amount));
        }

        pub fn decimals(env: Env) -> u32 {
            env.storage().instance().get(&TokenKey::Decimals).unwrap()
        }
    }
}

mod mock_oracle {
    use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

    use crate::{Asset, PriceData};

    #[contracttype]
    enum OracleKey {
        Price(Asset),
    }

    /// SEP-40 subset. Prices are stored with their observation timestamp.
    #[contract]
    pub struct MockOracle;

    #[contractimpl]
    impl MockOracle {
        /// Quote `token` at `price`, observed now.
        pub fn set_price(env: Env, token: Address, price: i128) {
            let timestamp = env.ledger().timestamp();
            Self::set_price_at(env, token, price, timestamp);
        }

        pub fn set_price_at(env: Env, token: Address, price: i128, timestamp: u64) {
            env.storage().instance().set(
                &OracleKey::Price(Asset::Stellar(token)),
                &PriceData { price, timestamp },
            );
        }

        pub fn decimals(_env: Env) -> u32 {
            14
        }

        pub fn lastprice(env: Env, asset: Asset) -> Option<PriceData> {
            env.storage().instance().get(&OracleKey::Price(asset))
        }
    }
}
