extern crate std;

use std::vec::Vec as StdVec;

use proptest::prelude::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    vec, Address, Env, IntoVal, String, Symbol, Vec,
};

use crate::testutils::{MockOracle, MockOracleClient, MockToken, MockTokenClient};
use crate::{BasketParams, ComponentInit, Error, IndexBasket, IndexBasketClient, BPS, SHARE};

/// Oracle prices use 14 decimals (Reflector convention).
const USD: i128 = 100_000_000_000_000;
const MAX_AGE: u64 = 3_600;
const TOL: u32 = 50;
const INCENTIVE: u32 = 10;

struct Setup<'a> {
    env: Env,
    admin: Address,
    arb: Address,
    oracle: MockOracleClient<'a>,
    tokens: StdVec<MockTokenClient<'a>>,
    decimals: StdVec<u32>,
    basket: IndexBasketClient<'a>,
}

fn deploy_tokens<'a>(
    env: &Env,
    oracle: &MockOracleClient<'a>,
    decimals: &[u32],
    prices: &[i128],
) -> StdVec<MockTokenClient<'a>> {
    decimals
        .iter()
        .zip(prices)
        .map(|(d, p)| {
            let t = MockTokenClient::new(env, &env.register(MockToken, (*d,)));
            oracle.set_price(&t.address, p);
            t
        })
        .collect()
}

fn params(
    env: &Env,
    admin: &Address,
    oracle: &Address,
    tokens: &[Address],
    weights: &[u32],
) -> BasketParams {
    let mut components = Vec::new(env);
    for (t, w) in tokens.iter().zip(weights) {
        components.push_back(ComponentInit {
            token: t.clone(),
            weight_bps: *w,
        });
    }
    BasketParams {
        admin: admin.clone(),
        name: String::from_str(env, "Stellar Index"),
        symbol: String::from_str(env, "SIDX"),
        components,
        oracle: oracle.clone(),
        initial_nav: 100 * USD,
        max_price_age: MAX_AGE,
        tolerance_bps: TOL,
        incentive_bps: INCENTIVE,
    }
}

fn new_env() -> Env {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| {
        l.timestamp = 1_700_000_000;
        l.sequence_number = 1_000;
    });
    env
}

fn setup<'a>(decimals: &[u32], weights: &[u32], prices: &[i128]) -> Setup<'a> {
    let env = new_env();
    let admin = Address::generate(&env);
    let arb = Address::generate(&env);
    let oracle = MockOracleClient::new(&env, &env.register(MockOracle, ()));
    let tokens = deploy_tokens(&env, &oracle, decimals, prices);
    let addrs: StdVec<Address> = tokens.iter().map(|t| t.address.clone()).collect();
    let p = params(&env, &admin, &oracle.address, &addrs, weights);
    let basket = IndexBasketClient::new(&env, &env.register(IndexBasket, (p,)));
    basket.set_arbitrageur(&arb, &true);
    Setup {
        env,
        admin,
        arb,
        oracle,
        tokens,
        decimals: decimals.to_vec(),
        basket,
    }
}

/// XLM (7) · USDC (6) · sBTC (8) · ETH (18) · a 2-decimal token, 20% each.
const FIVE_DEC: [u32; 5] = [7, 6, 8, 18, 2];
const FIVE_W: [u32; 5] = [2_000; 5];
fn five_prices() -> [i128; 5] {
    [USD / 10, USD, 60_000 * USD, 3_000 * USD, 5 * USD]
}

fn five<'a>() -> Setup<'a> {
    setup(&FIVE_DEC, &FIVE_W, &five_prices())
}

impl<'a> Setup<'a> {
    fn n(&self) -> u32 {
        self.tokens.len() as u32
    }

    fn fund(&self, who: &Address, amounts: &Vec<i128>) {
        for (t, a) in self.tokens.iter().zip(amounts.iter()) {
            t.mint(who, &a);
        }
    }

    fn unbounded(&self) -> Vec<i128> {
        let mut v = Vec::new(&self.env);
        for _ in 0..self.n() {
            v.push_back(i128::MAX);
        }
        v
    }

    fn zeros(&self) -> Vec<i128> {
        let mut v = Vec::new(&self.env);
        for _ in 0..self.n() {
            v.push_back(0);
        }
        v
    }

    /// Funds `who` with exactly what minting `amount` costs, then mints.
    fn issue(&self, who: &Address, amount: i128) -> Vec<i128> {
        let q = self.basket.quote_issue(&amount);
        self.fund(who, &q);
        self.basket.issue(who, &amount, &self.unbounded())
    }

    fn balances(&self, who: &Address) -> StdVec<i128> {
        self.tokens.iter().map(|t| t.balance(who)).collect()
    }

    /// Internal accounting equals real custody.
    fn assert_custody(&self) {
        let held = self.balances(&self.basket.address);
        let reserves: StdVec<i128> = self.basket.reserves().iter().collect();
        assert_eq!(held, reserves);
    }
}

fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(e as u32)
}

fn pow10(d: u32) -> i128 {
    10i128.pow(d)
}

// -------------------------------------------------------------------------
// Construction
// -------------------------------------------------------------------------

#[test]
fn constructor_derives_units_across_precisions() {
    let s = five();
    let prices = five_prices();
    let comps = s.basket.components();
    // $100 NAV * 20% = $20 of each component per whole share.
    for (i, c) in comps.iter().enumerate() {
        assert_eq!(c.decimals, FIVE_DEC[i]);
        assert_eq!(c.weight_bps, 2_000);
        assert_eq!(c.units, 20 * USD * pow10(FIVE_DEC[i]) / prices[i]);
    }
    assert_eq!(comps.get(0).unwrap().units, 200 * pow10(7)); // 200 XLM
    assert_eq!(comps.get(2).unwrap().units, 33_333); // 0.00033333 sBTC
    assert_eq!(comps.get(4).unwrap().units, 400); // 4.00 of the 2-dec token

    assert_eq!(s.basket.decimals(), 7);
    assert_eq!(s.basket.name(), String::from_str(&s.env, "Stellar Index"));
    assert_eq!(s.basket.symbol(), String::from_str(&s.env, "SIDX"));
    assert_eq!(s.basket.total_supply(), 0);
    assert_eq!(s.basket.admin(), s.admin);
}

#[test]
fn launch_weights_match_targets_50_30_20() {
    // XLM / USDC / sBTC, the canonical example.
    let s = setup(
        &[7, 6, 8],
        &[5_000, 3_000, 2_000],
        &[USD / 10, USD, 60_000 * USD],
    );
    s.issue(&Address::generate(&s.env), 10 * SHARE);
    let w = s.basket.weights();
    for (got, want) in w.iter().zip([5_000u32, 3_000, 2_000]) {
        assert!(got.abs_diff(want) <= 1, "{got} vs {want}");
    }
    // NAV ~ 10 shares * $100.
    let nav = s.basket.nav();
    assert!((1_000 * USD - nav).abs() <= USD / 100);
}

fn expect_ctor_error(tokens_decimals: &[u32], weights: &[u32], f: impl FnOnce(&mut BasketParams)) {
    let env = new_env();
    let admin = Address::generate(&env);
    let oracle = MockOracleClient::new(&env, &env.register(MockOracle, ()));
    let prices: StdVec<i128> = tokens_decimals.iter().map(|_| USD).collect();
    let tokens = deploy_tokens(&env, &oracle, tokens_decimals, &prices);
    let addrs: StdVec<Address> = tokens.iter().map(|t| t.address.clone()).collect();
    let mut p = params(&env, &admin, &oracle.address, &addrs, weights);
    f(&mut p);
    env.register(IndexBasket, (p,));
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn ctor_rejects_weights_not_summing_to_bps() {
    expect_ctor_error(&[7, 7], &[5_000, 4_000], |_| {});
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn ctor_rejects_zero_weight() {
    expect_ctor_error(&[7, 7, 7], &[5_000, 5_000, 0], |_| {});
}

#[test]
#[should_panic(expected = "Error(Contract, #2)")]
fn ctor_rejects_single_component() {
    expect_ctor_error(&[7], &[10_000], |_| {});
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn ctor_rejects_duplicate_component() {
    expect_ctor_error(&[7, 7], &[5_000, 5_000], |p| {
        let first = p.components.get(0).unwrap();
        p.components.set(
            1,
            ComponentInit {
                token: first.token,
                weight_bps: 5_000,
            },
        );
    });
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn ctor_rejects_tolerance_wider_than_a_weight() {
    expect_ctor_error(&[7, 7], &[9_950, 50], |_| {});
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn ctor_rejects_excessive_incentive() {
    expect_ctor_error(&[7, 7], &[5_000, 5_000], |p| p.incentive_bps = 501);
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn ctor_rejects_token_precision_above_18() {
    expect_ctor_error(&[7, 19], &[5_000, 5_000], |_| {});
}

#[test]
#[should_panic(expected = "Error(Contract, #17)")]
fn ctor_requires_oracle_price() {
    expect_ctor_error(&[7, 7], &[5_000, 5_000], |p| {
        p.components.set(
            1,
            ComponentInit {
                // A real token the oracle has never quoted.
                token: p.admin.env().register(MockToken, (7u32,)),
                weight_bps: 5_000,
            },
        );
    });
}

// -------------------------------------------------------------------------
// Issuance / redemption
// -------------------------------------------------------------------------

#[test]
fn first_issue_pulls_units_exactly() {
    let s = five();
    let user = Address::generate(&s.env);
    let pulled = s.issue(&user, SHARE);
    let units: StdVec<i128> = s.basket.components().iter().map(|c| c.units).collect();
    assert_eq!(pulled.iter().collect::<StdVec<_>>(), units);
    assert_eq!(s.basket.balance(&user), SHARE);
    assert_eq!(s.basket.total_supply(), SHARE);
    assert_eq!(s.balances(&user), [0; 5]);
    s.assert_custody();
}

#[test]
fn dust_issue_rounds_every_deposit_up() {
    let s = five();
    s.issue(&Address::generate(&s.env), SHARE);
    let user = Address::generate(&s.env);
    // 1 base unit = 1e-7 share: the 2-dec and 8-dec legs would be < 1 base unit.
    let pulled = s.issue(&user, 1);
    for a in pulled.iter() {
        assert!(a >= 1, "every component must be paid for, got {a}");
    }
    assert_eq!(pulled.get(4).unwrap(), 1); // ceil(400 / 1e7)
    assert_eq!(pulled.get(3).unwrap(), 20 * pow10(18) / 3_000 / SHARE + 1);
    // Redeeming it straight back returns nothing for the coarse legs: no free value.
    let out = s.basket.redeem(&user, &1, &s.zeros());
    for (o, p) in out.iter().zip(pulled.iter()) {
        assert!(o <= p);
    }
    assert_eq!(out.get(4).unwrap(), 0);
    s.assert_custody();
}

#[test]
fn subsequent_issue_follows_reserve_ratio() {
    let s = five();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    s.issue(&a, 3 * SHARE);
    let reserves = s.basket.reserves();
    let pulled = s.issue(&b, SHARE);
    for (p, r) in pulled.iter().zip(reserves.iter()) {
        assert_eq!(p, (r + 2) / 3); // ceil(r * 1 / 3)
    }
    assert_eq!(s.basket.total_supply(), 4 * SHARE);
    s.assert_custody();
}

#[test]
fn redeem_releases_pro_rata_and_full_exit_drains_reserves() {
    let s = five();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let paid_a = s.issue(&a, 7 * SHARE);
    s.issue(&b, 3 * SHARE);

    let reserves = s.basket.reserves();
    let out_b = s.basket.redeem(&b, &(3 * SHARE), &s.zeros());
    for (o, r) in out_b.iter().zip(reserves.iter()) {
        assert_eq!(o, r * 3 / 10);
    }
    assert_eq!(s.balances(&b), out_b.iter().collect::<StdVec<_>>());

    let reserves = s.basket.reserves();
    let out_a = s.basket.redeem(&a, &(7 * SHARE), &s.zeros());
    assert_eq!(out_a, reserves);
    for (o, p) in out_a.iter().zip(paid_a.iter()) {
        assert!(o >= p); // a absorbs b's rounding dust
    }
    assert_eq!(s.basket.total_supply(), 0);
    assert_eq!(s.basket.reserves(), s.zeros());
    s.assert_custody();

    // An emptied basket issues again at the stored units.
    let c = Address::generate(&s.env);
    let pulled = s.issue(&c, SHARE);
    assert_eq!(pulled.get(0).unwrap(), 200 * pow10(7));
}

#[test]
fn redemption_is_all_or_nothing() {
    let s = five();
    let user = Address::generate(&s.env);
    s.issue(&user, 5 * SHARE);
    let reserves = s.basket.reserves();

    // The fourth transfer fails after three succeeded: everything must revert.
    s.tokens[3].set_frozen(&true);
    let r = s.basket.try_redeem(&user, &(2 * SHARE), &s.zeros());
    assert!(r.is_err());

    assert_eq!(s.basket.balance(&user), 5 * SHARE);
    assert_eq!(s.basket.total_supply(), 5 * SHARE);
    assert_eq!(s.basket.reserves(), reserves);
    assert_eq!(s.balances(&user), [0; 5]);
    s.assert_custody();

    s.tokens[3].set_frozen(&false);
    s.basket.redeem(&user, &(2 * SHARE), &s.zeros());
    assert!(s.balances(&user).iter().all(|b| *b > 0));
    s.assert_custody();
}

#[test]
fn issuance_is_all_or_nothing() {
    let s = five();
    let user = Address::generate(&s.env);
    let q = s.basket.quote_issue(&SHARE);
    s.fund(&user, &q);
    s.tokens[4].set_frozen(&true);
    assert!(s.basket.try_issue(&user, &SHARE, &s.unbounded()).is_err());
    assert_eq!(s.basket.total_supply(), 0);
    assert_eq!(s.basket.balance(&user), 0);
    assert_eq!(s.balances(&user), q.iter().collect::<StdVec<_>>());
}

#[test]
fn slippage_bounds_are_enforced() {
    let s = five();
    let user = Address::generate(&s.env);
    let mut max_in = s.basket.quote_issue(&SHARE);
    s.fund(&user, &max_in);
    max_in.set(2, max_in.get(2).unwrap() - 1);
    assert_eq!(
        s.basket.try_issue(&user, &SHARE, &max_in),
        Err(Ok(err(Error::SlippageExceeded)))
    );
    s.basket.issue(&user, &SHARE, &s.unbounded());

    let mut min_out = s.basket.quote_redeem(&SHARE);
    min_out.set(0, min_out.get(0).unwrap() + 1);
    assert_eq!(
        s.basket.try_redeem(&user, &SHARE, &min_out),
        Err(Ok(err(Error::SlippageExceeded)))
    );
    let exact = s.basket.quote_redeem(&SHARE);
    assert_eq!(s.basket.redeem(&user, &SHARE, &exact), exact);
}

#[test]
fn issue_and_redeem_input_validation() {
    let s = five();
    let user = Address::generate(&s.env);
    assert_eq!(
        s.basket.try_issue(&user, &0, &s.unbounded()),
        Err(Ok(err(Error::InvalidAmount)))
    );
    assert_eq!(
        s.basket.try_issue(&user, &SHARE, &vec![&s.env, i128::MAX]),
        Err(Ok(err(Error::LengthMismatch)))
    );
    s.issue(&user, SHARE);
    assert_eq!(
        s.basket.try_redeem(&user, &(SHARE + 1), &s.zeros()),
        Err(Ok(err(Error::InsufficientBalance)))
    );
    assert_eq!(
        s.basket.try_redeem(&user, &-1, &s.zeros()),
        Err(Ok(err(Error::InvalidAmount)))
    );
}

#[test]
fn issue_requires_depositor_auth() {
    let s = five();
    let user = Address::generate(&s.env);
    s.issue(&user, SHARE);
    let auths = s.env.auths();
    assert_eq!(auths[0].0, user);
    assert_eq!(
        auths[0].1.function,
        soroban_sdk::testutils::AuthorizedFunction::Contract((
            s.basket.address.clone(),
            Symbol::new(&s.env, "issue"),
            (user.clone(), SHARE, s.unbounded()).into_val(&s.env),
        ))
    );
}

#[test]
fn donations_do_not_move_issuance_ratio() {
    let s = five();
    let user = Address::generate(&s.env);
    s.issue(&user, SHARE);
    let before = s.basket.quote_issue(&SHARE);
    s.tokens[2].mint(&s.basket.address, &1_000_000_000);
    assert_eq!(s.basket.quote_issue(&SHARE), before);
}

// -------------------------------------------------------------------------
// SEP-41 surface
// -------------------------------------------------------------------------

#[test]
fn basket_token_transfers_and_allowances() {
    let s = five();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let spender = Address::generate(&s.env);
    s.issue(&a, 10 * SHARE);

    s.basket.transfer(&a, &b, &(4 * SHARE));
    assert_eq!(s.basket.balance(&a), 6 * SHARE);
    assert_eq!(s.basket.balance(&b), 4 * SHARE);

    s.basket.approve(&a, &spender, &SHARE, &2_000);
    assert_eq!(s.basket.allowance(&a, &spender), SHARE);
    s.basket.transfer_from(&spender, &a, &b, &(SHARE / 2));
    assert_eq!(s.basket.allowance(&a, &spender), SHARE / 2);
    assert_eq!(
        s.basket.try_transfer_from(&spender, &a, &b, &SHARE),
        Err(Ok(err(Error::InsufficientAllowance)))
    );
    s.env.ledger().with_mut(|l| l.sequence_number = 2_001);
    assert_eq!(s.basket.allowance(&a, &spender), 0);
    assert_eq!(
        s.basket.try_approve(&a, &spender, &SHARE, &1_000),
        Err(Ok(err(Error::InvalidExpiration)))
    );

    // Transferred basket tokens redeem like any other.
    let out = s.basket.redeem(&b, &(4 * SHARE + SHARE / 2), &s.zeros());
    assert!(out.iter().all(|o| o > 0));
    s.assert_custody();
}

#[test]
fn token_burn_shrinks_supply_and_accrues_to_holders() {
    let s = five();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    s.issue(&a, SHARE);
    s.issue(&b, SHARE);
    s.basket.burn(&a, &SHARE);
    assert_eq!(s.basket.total_supply(), SHARE);
    assert_eq!(s.basket.quote_redeem(&SHARE), s.basket.reserves());
}

// -------------------------------------------------------------------------
// Rebalancing
// -------------------------------------------------------------------------

/// Plays the arbitrageur: repeatedly pairs the most overweight and most
/// underweight components and trades the value needed to close the smaller
/// gap. Returns the number of trades executed.
fn arbitrage_to_targets(s: &Setup) -> u32 {
    let prices: StdVec<i128> = s
        .tokens
        .iter()
        .map(|t| {
            s.oracle
                .lastprice(&crate::Asset::Stellar(t.address.clone()))
                .unwrap()
                .price
        })
        .collect();
    let targets: StdVec<u32> = s.basket.components().iter().map(|c| c.weight_bps).collect();
    let mut trades = 0;
    for _ in 0..20 {
        let w: StdVec<i128> = s.basket.weights().iter().map(|w| w as i128).collect();
        let drift: StdVec<i128> = w
            .iter()
            .zip(&targets)
            .map(|(w, t)| w - *t as i128)
            .collect();
        if drift.iter().all(|d| d.abs() <= TOL as i128) {
            break;
        }
        let o = (0..drift.len()).max_by_key(|i| drift[*i]).unwrap();
        let u = (0..drift.len()).min_by_key(|i| drift[*i]).unwrap();
        let gap_bps = drift[o].min(-drift[u]);
        // Value to move, trimmed for the arbitrage premium and rounding.
        let move_value =
            s.basket.nav() * gap_bps / BPS as i128 * (BPS - INCENTIVE - 5) as i128 / BPS as i128;
        let amount_in = move_value * pow10(s.decimals[u]) / prices[u];
        let token_in = &s.tokens[u];
        token_in.mint(&s.arb, &amount_in);
        let quote = s
            .basket
            .rebalance_quote(&token_in.address, &s.tokens[o].address, &amount_in);
        let before_out = s.tokens[o].balance(&s.arb);
        let got = s.basket.rebalance(
            &s.arb,
            &token_in.address,
            &s.tokens[o].address,
            &amount_in,
            &quote,
        );
        assert_eq!(got, quote);
        assert_eq!(s.tokens[o].balance(&s.arb) - before_out, got);
        trades += 1;
    }
    trades
}

#[test]
fn five_asset_basket_restores_20pct_weights_after_price_drift() {
    let s = five();
    let holders: StdVec<Address> = (0..3).map(|_| Address::generate(&s.env)).collect();
    for (i, h) in holders.iter().enumerate() {
        s.issue(h, (i as i128 + 1) * 250 * SHARE);
    }
    let supply = s.basket.total_supply();

    // Simulate drift: XLM doubles, sBTC halves, the 2-dec token +30%.
    let p = five_prices();
    s.oracle.set_price(&s.tokens[0].address, &(p[0] * 2));
    s.oracle.set_price(&s.tokens[2].address, &(p[2] / 2));
    s.oracle.set_price(&s.tokens[4].address, &(p[4] * 13 / 10));
    let drifted = s.basket.weights();
    assert!(drifted.get(0).unwrap() > 3_000);
    assert!(drifted.get(2).unwrap() < 1_200);
    let nav_before = s.basket.nav();

    let trades = arbitrage_to_targets(&s);
    assert!(trades >= 2);

    for w in s.basket.weights().iter() {
        assert!(w.abs_diff(2_000) <= TOL, "weight {w} outside 20% ± band");
    }
    // Holders only paid the capped premium on the value moved.
    let nav_after = s.basket.nav();
    assert!(nav_after <= nav_before);
    assert!(nav_before - nav_after <= nav_before * INCENTIVE as i128 / BPS as i128);
    assert_eq!(s.basket.total_supply(), supply);
    s.assert_custody();

    // Units follow the rebalanced reserves, and redemption still pays out all legs.
    for (c, r) in s.basket.components().iter().zip(s.basket.reserves().iter()) {
        assert_eq!(c.units, r * SHARE / supply);
    }
    let out = s.basket.redeem(&holders[2], &(750 * SHARE), &s.zeros());
    assert!(out.iter().all(|o| o > 0));
    s.assert_custody();

    // Nothing left to arbitrage.
    let t0 = &s.tokens[0];
    let t2 = &s.tokens[2];
    t2.mint(&s.arb, &1_000);
    assert_eq!(
        s.basket
            .try_rebalance(&s.arb, &t2.address, &t0.address, &1_000, &0),
        Err(Ok(err(Error::NotDrifted)))
    );
}

#[test]
fn rebalance_converts_across_decimal_offsets() {
    // 6-dec stablecoin vs 18-dec asset: a 12-decimal offset.
    let s = setup(&[6, 18], &[5_000, 5_000], &[USD, 2_000 * USD]);
    let user = Address::generate(&s.env);
    s.issue(&user, 100 * SHARE);
    s.oracle.set_price(&s.tokens[1].address, &(2_400 * USD));

    let amount_in = 100 * pow10(6); // $100 of the stablecoin
    s.tokens[0].mint(&s.arb, &amount_in);
    let out = s.basket.rebalance(
        &s.arb,
        &s.tokens[0].address,
        &s.tokens[1].address,
        &amount_in,
        &0,
    );
    // $100 / $2400 * 1.001 = 0.0417083333... of the 18-dec asset.
    assert_eq!(out, 41_708_333_333_333_333);
    assert_eq!(s.tokens[1].balance(&s.arb), out);
    s.assert_custody();
}

#[test]
fn rebalance_handles_huge_18_decimal_reserves() {
    // Intermediate products exceed i128 and go through 256-bit math.
    let s = setup(&[18, 18], &[5_000, 5_000], &[USD, USD]);
    let user = Address::generate(&s.env);
    s.issue(&user, 1_000_000_000 * SHARE); // 5e28 base units per leg
    s.oracle.set_price(&s.tokens[0].address, &(USD * 12 / 10));
    let amount_in = 1_000_000 * pow10(18);
    s.tokens[1].mint(&s.arb, &amount_in);
    let out = s.basket.rebalance(
        &s.arb,
        &s.tokens[1].address,
        &s.tokens[0].address,
        &amount_in,
        &0,
    );
    assert_eq!(out, amount_in * 1_001 / 1_000 * 10 / 12);
    s.assert_custody();
}

fn drifted_pair<'a>() -> Setup<'a> {
    let s = setup(&[7, 8], &[5_000, 5_000], &[USD, 50_000 * USD]);
    s.issue(&Address::generate(&s.env), 1_000 * SHARE);
    // Token 1 up 20%: token 1 overweight (~54.5%), token 0 underweight.
    s.oracle.set_price(&s.tokens[1].address, &(60_000 * USD));
    s.tokens[0].mint(&s.arb, &(1_000_000 * pow10(7)));
    s.tokens[1].mint(&s.arb, &(1_000 * pow10(8)));
    s
}

#[test]
fn rebalance_requires_authorised_arbitrageur() {
    let s = drifted_pair();
    let stranger = Address::generate(&s.env);
    s.tokens[0].mint(&stranger, &pow10(9));
    assert_eq!(
        s.basket.try_rebalance(
            &stranger,
            &s.tokens[0].address,
            &s.tokens[1].address,
            &pow10(9),
            &0
        ),
        Err(Ok(err(Error::NotArbitrageur)))
    );
    s.basket.set_arbitrageur(&s.arb, &false);
    assert!(!s.basket.is_arbitrageur(&s.arb));
    assert_eq!(
        s.basket.try_rebalance(
            &s.arb,
            &s.tokens[0].address,
            &s.tokens[1].address,
            &pow10(9),
            &0
        ),
        Err(Ok(err(Error::NotArbitrageur)))
    );
}

#[test]
fn set_arbitrageur_requires_admin_auth() {
    let s = five();
    let who = Address::generate(&s.env);
    s.basket.set_arbitrageur(&who, &true);
    assert_eq!(s.env.auths()[0].0, s.admin);
    assert!(s.basket.is_arbitrageur(&who));
}

#[test]
fn rebalance_rejects_wrong_direction_and_in_band_trades() {
    let s = drifted_pair();
    let (t0, t1) = (&s.tokens[0].address, &s.tokens[1].address);
    // Selling the underweight asset out of the basket.
    assert_eq!(
        s.basket.try_rebalance(&s.arb, t1, t0, &pow10(8), &0),
        Err(Ok(err(Error::NotDrifted)))
    );
    // Drift inside the 0.5% band is not actionable.
    s.oracle.set_price(t1, &(50_400 * USD)); // ~50.2% / 49.8%
    assert_eq!(
        s.basket.try_rebalance(&s.arb, t0, t1, &pow10(9), &0),
        Err(Ok(err(Error::NotDrifted)))
    );
}

#[test]
fn rebalance_rejects_overshoot() {
    let s = drifted_pair();
    let (t0, t1) = (&s.tokens[0].address, &s.tokens[1].address);
    // Gap is ~$4.5k of $110k; bringing in $20k overshoots the target.
    assert_eq!(
        s.basket
            .try_rebalance(&s.arb, t0, t1, &(20_000 * pow10(7)), &0),
        Err(Ok(err(Error::Overshoot)))
    );
    // Exactly half the value gap lands on target.
    let out = s.basket.rebalance(&s.arb, t0, t1, &(4_500 * pow10(7)), &0);
    assert!(out > 0);
    for w in s.basket.weights().iter() {
        assert!(w.abs_diff(5_000) <= TOL);
    }
}

#[test]
fn rebalance_rejects_stale_or_missing_prices() {
    let s = drifted_pair();
    let (t0, t1) = (&s.tokens[0].address, &s.tokens[1].address);
    let now = s.env.ledger().timestamp();
    s.oracle
        .set_price_at(t1, &(60_000 * USD), &(now - MAX_AGE - 1));
    assert_eq!(
        s.basket.try_rebalance(&s.arb, t0, t1, &pow10(9), &0),
        Err(Ok(err(Error::StalePrice)))
    );
    s.oracle.set_price(t1, &0);
    assert_eq!(
        s.basket.try_rebalance(&s.arb, t0, t1, &pow10(9), &0),
        Err(Ok(err(Error::PriceUnavailable)))
    );
}

#[test]
fn rebalance_argument_validation() {
    let s = drifted_pair();
    let (t0, t1) = (&s.tokens[0].address, &s.tokens[1].address);
    let unknown = Address::generate(&s.env);
    assert_eq!(
        s.basket.try_rebalance(&s.arb, t0, t0, &pow10(9), &0),
        Err(Ok(err(Error::SameComponent)))
    );
    assert_eq!(
        s.basket.try_rebalance(&s.arb, &unknown, t1, &pow10(9), &0),
        Err(Ok(err(Error::UnknownComponent)))
    );
    assert_eq!(
        s.basket.try_rebalance(&s.arb, t0, t1, &0, &0),
        Err(Ok(err(Error::InvalidAmount)))
    );
    let quote = s.basket.rebalance_quote(t0, t1, &pow10(9));
    assert_eq!(
        s.basket
            .try_rebalance(&s.arb, t0, t1, &pow10(9), &(quote + 1)),
        Err(Ok(err(Error::SlippageExceeded)))
    );
    assert_eq!(
        s.basket.try_rebalance(&s.arb, t0, t1, &1, &0),
        Err(Ok(err(Error::AmountTooSmall)))
    );
}

#[test]
fn rebalance_on_empty_basket_fails() {
    let s = setup(&[7, 8], &[5_000, 5_000], &[USD, 50_000 * USD]);
    s.tokens[0].mint(&s.arb, &pow10(9));
    assert_eq!(
        s.basket.try_rebalance(
            &s.arb,
            &s.tokens[0].address,
            &s.tokens[1].address,
            &pow10(9),
            &0
        ),
        Err(Ok(err(Error::EmptyBasket)))
    );
}

#[test]
fn issuance_after_rebalance_uses_new_composition() {
    let s = drifted_pair();
    let (t0, t1) = (&s.tokens[0].address, &s.tokens[1].address);
    s.basket.rebalance(&s.arb, t0, t1, &(4_500 * pow10(7)), &0);
    let reserves = s.basket.reserves();
    let supply = s.basket.total_supply();
    let q = s.basket.quote_issue(&SHARE);
    for (a, r) in q.iter().zip(reserves.iter()) {
        assert_eq!(a, (r * SHARE + supply - 1) / supply);
    }
}

// -------------------------------------------------------------------------
// Property: collateralisation across precisions
// -------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Random issue/redeem/transfer sequences over tokens with random
    /// precisions never leave the basket short, and no round trip profits.
    #[test]
    fn prop_always_fully_collateralised(
        decimals in prop::collection::vec(prop::sample::select(std::vec![0u32, 2, 6, 7, 8, 12, 18]), 2..=5),
        ops in prop::collection::vec((0u8..3, 0usize..3, 1i128..5_000_000_000), 1..25),
    ) {
        let n = decimals.len();
        let weights: StdVec<u32> = {
            let mut w = std::vec![BPS / n as u32; n];
            w[0] += BPS - w.iter().sum::<u32>();
            w
        };
        let prices: StdVec<i128> = (0..n).map(|i| USD * (i as i128 * 3 + 1) / 4).collect();
        let s = setup(&decimals, &weights, &prices);
        let users: StdVec<Address> = (0..3).map(|_| Address::generate(&s.env)).collect();

        for (op, u, amount) in ops {
            let user = &users[u];
            match op {
                0 => {
                    let paid = s.issue(user, amount);
                    // Immediate round trip never returns more than was paid.
                    let q = s.basket.quote_redeem(&amount);
                    for (o, p) in q.iter().zip(paid.iter()) {
                        prop_assert!(o <= p);
                    }
                }
                1 => {
                    let bal = s.basket.balance(user);
                    let amt = amount.min(bal);
                    if amt > 0 {
                        let _ = s.basket.try_redeem(user, &amt, &s.zeros());
                    }
                }
                _ => {
                    let bal = s.basket.balance(user);
                    let to = &users[(u + 1) % 3];
                    s.basket.transfer(user, to, &amount.min(bal));
                }
            }
            // Sum of every holder's claim never exceeds custody.
            let supply = s.basket.total_supply();
            let reserves = s.basket.reserves();
            let mut claimed = std::vec![0i128; n];
            for h in &users {
                let b = s.basket.balance(h);
                if b > 0 {
                    for (i, c) in s.basket.quote_redeem(&b).iter().enumerate() {
                        claimed[i] += c;
                    }
                }
            }
            for (c, r) in claimed.iter().zip(reserves.iter()) {
                prop_assert!(*c <= r);
                if supply > 0 {
                    prop_assert!(r > 0);
                }
            }
            s.assert_custody();
        }
    }
}
