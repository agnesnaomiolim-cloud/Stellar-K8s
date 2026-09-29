//! SEP-41 interface of the basket token. Balances are the only claim on the
//! reserves; burning through this interface forfeits the underlying assets to
//! the remaining holders (use `redeem` to withdraw them).

use soroban_sdk::{
    contractimpl, panic_with_error, symbol_short, token::TokenInterface, Address, Env, String,
};

use crate::{storage, Error, IndexBasket, IndexBasketArgs, IndexBasketClient, SHARE_DECIMALS};

fn check_amount(env: &Env, amount: i128) {
    if amount < 0 {
        panic_with_error!(env, Error::InvalidAmount);
    }
}

fn spend_balance(env: &Env, from: &Address, amount: i128) {
    let balance = storage::balance(env, from);
    if balance < amount {
        panic_with_error!(env, Error::InsufficientBalance);
    }
    storage::set_balance(env, from, balance - amount);
}

fn move_balance(env: &Env, from: &Address, to: &Address, amount: i128) {
    spend_balance(env, from, amount);
    let to_balance = storage::balance(env, to);
    storage::set_balance(
        env,
        to,
        to_balance
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(env, Error::Overflow)),
    );
}

fn spend_allowance(env: &Env, from: &Address, spender: &Address, amount: i128) {
    let allowance = storage::allowance(env, from, spender);
    if allowance.amount < amount {
        panic_with_error!(env, Error::InsufficientAllowance);
    }
    if amount > 0 {
        storage::set_allowance(
            env,
            from,
            spender,
            allowance.amount - amount,
            allowance.expiration_ledger,
        );
    }
}

fn burn_supply(env: &Env, from: &Address, amount: i128) {
    spend_balance(env, from, amount);
    storage::set_supply(env, storage::supply(env) - amount);
    env.events()
        .publish((symbol_short!("burn"), from.clone()), amount);
}

#[contractimpl]
impl TokenInterface for IndexBasket {
    fn allowance(env: Env, from: Address, spender: Address) -> i128 {
        storage::allowance(&env, &from, &spender).amount
    }

    fn approve(env: Env, from: Address, spender: Address, amount: i128, expiration_ledger: u32) {
        from.require_auth();
        check_amount(&env, amount);
        storage::extend_instance(&env);
        storage::set_allowance(&env, &from, &spender, amount, expiration_ledger);
        env.events().publish(
            (symbol_short!("approve"), from, spender),
            (amount, expiration_ledger),
        );
    }

    fn balance(env: Env, id: Address) -> i128 {
        storage::balance(&env, &id)
    }

    fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        check_amount(&env, amount);
        storage::extend_instance(&env);
        move_balance(&env, &from, &to, amount);
        env.events()
            .publish((symbol_short!("transfer"), from, to), amount);
    }

    fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        spender.require_auth();
        check_amount(&env, amount);
        storage::extend_instance(&env);
        spend_allowance(&env, &from, &spender, amount);
        move_balance(&env, &from, &to, amount);
        env.events()
            .publish((symbol_short!("transfer"), from, to), amount);
    }

    fn burn(env: Env, from: Address, amount: i128) {
        from.require_auth();
        check_amount(&env, amount);
        storage::extend_instance(&env);
        burn_supply(&env, &from, amount);
    }

    fn burn_from(env: Env, spender: Address, from: Address, amount: i128) {
        spender.require_auth();
        check_amount(&env, amount);
        storage::extend_instance(&env);
        spend_allowance(&env, &from, &spender, amount);
        burn_supply(&env, &from, amount);
    }

    fn decimals(_env: Env) -> u32 {
        SHARE_DECIMALS
    }

    fn name(env: Env) -> String {
        storage::metadata(&env).name
    }

    fn symbol(env: Env) -> String {
        storage::metadata(&env).symbol
    }
}
