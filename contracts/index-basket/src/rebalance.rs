//! Arbitrageur-driven rebalancing.
//!
//! Price moves change each component's share of basket value while the held
//! quantities stay put. An authorised arbitrageur restores the targets by
//! delivering an **underweight** component and taking an **overweight** one,
//! priced by the SEP-40 oracle plus a capped `incentive_bps` premium.
//!
//! A trade is accepted only when it corrects real drift, never overshoots:
//!
//! 1. `token_in` is below its target weight and `token_out` above its own;
//! 2. at least one of the two sits outside the `tolerance_bps` band;
//! 3. afterwards `token_in` is at most `target + tolerance` and `token_out`
//!    at least `target - tolerance`.
//!
//! Basket supply is untouched, so each holder's claim shifts from the
//! overweight asset to the underweight one at (near) equal value. After the
//! trade the per-share `units` of both legs are refreshed from reserves.

use soroban_sdk::{contractimpl, panic_with_error, symbol_short, token, Address, Env, Vec};

use crate::math::{cmp_products, mul_div, mul_div_many, pow10, value, Rounding};
use crate::{
    oracle, storage, Component, Error, IndexBasket, IndexBasketArgs, IndexBasketClient,
    RebalanceConfig, BPS, SHARE,
};
use core::cmp::Ordering;

/// Oracle-priced view of the basket.
struct Valuation {
    prices: Vec<i128>,
    values: Vec<i128>,
    total: i128,
}

fn valuation(
    env: &Env,
    cfg: &RebalanceConfig,
    components: &Vec<Component>,
    reserves: &Vec<i128>,
) -> Valuation {
    let mut prices = Vec::new(env);
    let mut values = Vec::new(env);
    let mut total: i128 = 0;
    for (c, r) in components.iter().zip(reserves.iter()) {
        let p = oracle::fresh_price(env, cfg, &c.token);
        let v = value(env, r, p, c.decimals);
        total = total
            .checked_add(v)
            .unwrap_or_else(|| panic_with_error!(env, Error::Overflow));
        prices.push_back(p);
        values.push_back(v);
    }
    Valuation {
        prices,
        values,
        total,
    }
}

fn index_of(env: &Env, components: &Vec<Component>, token: &Address) -> u32 {
    components
        .iter()
        .position(|c| c.token == *token)
        .map(|i| i as u32)
        .unwrap_or_else(|| panic_with_error!(env, Error::UnknownComponent))
}

/// `value / total` compared with `bps / BPS`.
fn weight_cmp(env: &Env, value: i128, total: i128, bps: u32) -> Ordering {
    cmp_products(env, value, BPS as i128, total, bps as i128)
}

/// Oracle conversion of `amount_in` of `c_in` into `c_out`, including the
/// arbitrage premium, rounded down.
fn convert(
    env: &Env,
    cfg: &RebalanceConfig,
    c_in: &Component,
    p_in: i128,
    c_out: &Component,
    p_out: i128,
    amount_in: i128,
) -> i128 {
    mul_div_many(
        env,
        &[
            amount_in,
            p_in,
            pow10(env, c_out.decimals),
            (BPS + cfg.incentive_bps) as i128,
        ],
        &[p_out, pow10(env, c_in.decimals), BPS as i128],
        Rounding::Down,
    )
}

#[contractimpl]
impl IndexBasket {
    /// Swaps `amount_in` of the underweight `token_in` (paid by the
    /// arbitrageur) for the oracle-equivalent amount of the overweight
    /// `token_out` plus the incentive. Returns the amount paid out.
    pub fn rebalance(
        env: Env,
        arbitrageur: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_amount_out: i128,
    ) -> i128 {
        arbitrageur.require_auth();
        if !storage::is_arbitrageur(&env, &arbitrageur) {
            panic_with_error!(&env, Error::NotArbitrageur);
        }
        if amount_in <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        storage::extend_instance(&env);

        let cfg = storage::config(&env);
        let mut components = storage::components(&env);
        let mut reserves = storage::reserves(&env);
        let supply = storage::supply(&env);
        if supply == 0 {
            panic_with_error!(&env, Error::EmptyBasket);
        }
        let i = index_of(&env, &components, &token_in);
        let o = index_of(&env, &components, &token_out);
        if i == o {
            panic_with_error!(&env, Error::SameComponent);
        }
        let mut c_in = components.get_unchecked(i);
        let mut c_out = components.get_unchecked(o);
        let val = valuation(&env, &cfg, &components, &reserves);
        let (v_in, v_out) = (val.values.get_unchecked(i), val.values.get_unchecked(o));
        let tol = cfg.tolerance_bps;

        // 1 & 2: the pair must be drifted, in the direction this trade fixes.
        let in_under = weight_cmp(&env, v_in, val.total, c_in.weight_bps) == Ordering::Less;
        let out_over = weight_cmp(&env, v_out, val.total, c_out.weight_bps) == Ordering::Greater;
        let outside_band = weight_cmp(&env, v_in, val.total, c_in.weight_bps - tol)
            == Ordering::Less
            || weight_cmp(&env, v_out, val.total, c_out.weight_bps + tol) == Ordering::Greater;
        if !(in_under && out_over && outside_band) {
            panic_with_error!(&env, Error::NotDrifted);
        }

        let (p_in, p_out) = (val.prices.get_unchecked(i), val.prices.get_unchecked(o));
        let amount_out = convert(&env, &cfg, &c_in, p_in, &c_out, p_out, amount_in);
        if amount_out == 0 {
            panic_with_error!(&env, Error::AmountTooSmall);
        }
        if amount_out < min_amount_out {
            panic_with_error!(&env, Error::SlippageExceeded);
        }
        let r_out = reserves.get_unchecked(o);
        if amount_out >= r_out {
            panic_with_error!(&env, Error::InsufficientReserve);
        }
        let new_r_in = reserves
            .get_unchecked(i)
            .checked_add(amount_in)
            .unwrap_or_else(|| panic_with_error!(&env, Error::Overflow));
        let new_r_out = r_out - amount_out;

        // 3: no overshoot past the band on either leg.
        let new_v_in = value(&env, new_r_in, p_in, c_in.decimals);
        let new_v_out = value(&env, new_r_out, p_out, c_out.decimals);
        let new_total = val.total - v_in - v_out + new_v_in + new_v_out;
        if weight_cmp(&env, new_v_in, new_total, c_in.weight_bps + tol) == Ordering::Greater
            || weight_cmp(&env, new_v_out, new_total, c_out.weight_bps - tol) == Ordering::Less
        {
            panic_with_error!(&env, Error::Overshoot);
        }

        // Effects: reserves and refreshed per-share units.
        reserves.set(i, new_r_in);
        reserves.set(o, new_r_out);
        c_in.units = mul_div(&env, new_r_in, SHARE, supply, Rounding::Down);
        c_out.units = mul_div(&env, new_r_out, SHARE, supply, Rounding::Down);
        components.set(i, c_in);
        components.set(o, c_out);
        storage::set_reserves(&env, &reserves);
        storage::set_components(&env, &components);

        // Interactions: both legs settle in this invocation or not at all.
        let this = env.current_contract_address();
        token::Client::new(&env, &token_in).transfer(&arbitrageur, &this, &amount_in);
        token::Client::new(&env, &token_out).transfer(&this, &arbitrageur, &amount_out);

        env.events().publish(
            (symbol_short!("rebalance"), arbitrageur),
            (token_in, token_out, amount_in, amount_out),
        );
        amount_out
    }

    /// `token_out` paid for `amount_in` of `token_in` at current oracle
    /// prices, before drift checks.
    pub fn rebalance_quote(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
    ) -> i128 {
        if amount_in <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }
        let cfg = storage::config(&env);
        let components = storage::components(&env);
        let i = index_of(&env, &components, &token_in);
        let o = index_of(&env, &components, &token_out);
        if i == o {
            panic_with_error!(&env, Error::SameComponent);
        }
        let (c_in, c_out) = (components.get_unchecked(i), components.get_unchecked(o));
        let p_in = oracle::fresh_price(&env, &cfg, &c_in.token);
        let p_out = oracle::fresh_price(&env, &cfg, &c_out.token);
        convert(&env, &cfg, &c_in, p_in, &c_out, p_out, amount_in)
    }

    /// Current value weight of each component in basis points (rounded down;
    /// all zero for an empty basket).
    pub fn weights(env: Env) -> Vec<u32> {
        let components = storage::components(&env);
        let val = valuation(
            &env,
            &storage::config(&env),
            &components,
            &storage::reserves(&env),
        );
        let mut out = Vec::new(&env);
        for v in val.values.iter() {
            out.push_back(if val.total == 0 {
                0
            } else {
                mul_div(&env, v, BPS as i128, val.total, Rounding::Down) as u32
            });
        }
        out
    }

    /// Oracle value of the full reserves, in the oracle's price scale.
    pub fn nav(env: Env) -> i128 {
        let components = storage::components(&env);
        valuation(
            &env,
            &storage::config(&env),
            &components,
            &storage::reserves(&env),
        )
        .total
    }
}
