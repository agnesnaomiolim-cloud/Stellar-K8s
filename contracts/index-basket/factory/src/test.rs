//! Requires the basket WASM: `make build` (or
//! `cargo build -p index-basket --target wasm32v1-none --release`) first.

extern crate std;

use std::vec::Vec as StdVec;

use index_basket::testutils::{MockOracle, MockOracleClient, MockToken, MockTokenClient};
use index_basket::{Asset, IndexBasketClient, BPS, SHARE};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Address, BytesN, Env, String, Vec,
};

use crate::{BasketFactory, BasketFactoryClient, BasketParams, ComponentInit, Error};

const BASKET_WASM: &[u8] = include_bytes!("../../target/wasm32v1-none/release/index_basket.wasm");
const USD: i128 = 100_000_000_000_000;
const TOL: u32 = 50;
const INCENTIVE: u32 = 10;
const DECIMALS: [u32; 5] = [7, 6, 8, 18, 2];
const PRICES: [i128; 5] = [USD / 10, USD, 60_000 * USD, 3_000 * USD, 5 * USD];

struct Setup<'a> {
    env: Env,
    admin: Address,
    factory: BasketFactoryClient<'a>,
    oracle: MockOracleClient<'a>,
    tokens: StdVec<MockTokenClient<'a>>,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_700_000_000);
    let admin = Address::generate(&env);
    let hash = env.deployer().upload_contract_wasm(BASKET_WASM);
    let factory = BasketFactoryClient::new(&env, &env.register(BasketFactory, (&admin, hash)));
    let oracle = MockOracleClient::new(&env, &env.register(MockOracle, ()));
    let tokens = DECIMALS
        .iter()
        .zip(PRICES)
        .map(|(d, p)| {
            let t = MockTokenClient::new(&env, &env.register(MockToken, (*d,)));
            oracle.set_price(&t.address, &p);
            t
        })
        .collect();
    Setup {
        env,
        admin,
        factory,
        oracle,
        tokens,
    }
}

fn params(s: &Setup, creator: &Address, weights: &[u32]) -> BasketParams {
    let mut components = Vec::new(&s.env);
    for (t, w) in s.tokens.iter().zip(weights) {
        components.push_back(ComponentInit {
            token: t.address.clone(),
            weight_bps: *w,
        });
    }
    BasketParams {
        admin: creator.clone(),
        name: String::from_str(&s.env, "Stellar Top 5"),
        symbol: String::from_str(&s.env, "ST5"),
        components,
        oracle: s.oracle.address.clone(),
        initial_nav: 100 * USD,
        max_price_age: 3_600,
        tolerance_bps: TOL,
        incentive_bps: INCENTIVE,
    }
}

fn salt(env: &Env, b: u8) -> BytesN<32> {
    BytesN::from_array(env, &[b; 32])
}

fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from_contract_error(e as u32)
}

#[test]
fn creates_and_registers_baskets() {
    let s = setup();
    let creator = Address::generate(&s.env);
    let expected = s.factory.basket_address(&creator, &salt(&s.env, 1));
    let basket = s.factory.create_basket(
        &creator,
        &salt(&s.env, 1),
        &params(&s, &creator, &[2_000; 5]),
    );
    assert_eq!(basket, expected);
    assert!(s.factory.is_basket(&basket));
    assert_eq!(s.factory.basket_count(), 1);

    // Same salt, different creator: a distinct, un-squattable address.
    let other = Address::generate(&s.env);
    let second = s.factory.create_basket(
        &other,
        &salt(&s.env, 1),
        &params(&s, &other, &[5_000, 3_000, 1_000, 500, 500]),
    );
    assert_ne!(second, basket);
    assert_eq!(s.factory.basket_count(), 2);
    assert_eq!(
        s.factory.baskets(&0, &10),
        Vec::from_array(&s.env, [basket.clone(), second.clone()])
    );
    assert_eq!(
        s.factory.baskets(&1, &10),
        Vec::from_array(&s.env, [second])
    );
    assert_eq!(s.factory.baskets(&5, &10).len(), 0);
    assert!(!s.factory.is_basket(&s.tokens[0].address));

    // Constructor arguments crossed the factory boundary intact.
    let b = IndexBasketClient::new(&s.env, &basket);
    assert_eq!(b.admin(), creator);
    assert_eq!(b.symbol(), String::from_str(&s.env, "ST5"));
    assert_eq!(b.components().len(), 5);
    assert_eq!(b.config().tolerance_bps, TOL);
}

#[test]
fn salt_reuse_by_same_creator_fails() {
    let s = setup();
    let creator = Address::generate(&s.env);
    let p = params(&s, &creator, &[2_000; 5]);
    s.factory.create_basket(&creator, &salt(&s.env, 7), &p);
    assert!(s
        .factory
        .try_create_basket(&creator, &salt(&s.env, 7), &p)
        .is_err());
    assert_eq!(s.factory.basket_count(), 1);
}

#[test]
fn creator_must_be_basket_admin() {
    let s = setup();
    let creator = Address::generate(&s.env);
    let p = params(&s, &Address::generate(&s.env), &[2_000; 5]);
    assert_eq!(
        s.factory.try_create_basket(&creator, &salt(&s.env, 1), &p),
        Err(Ok(err(Error::AdminMismatch)))
    );
}

#[test]
fn invalid_basket_reverts_whole_deployment() {
    let s = setup();
    let creator = Address::generate(&s.env);
    let bad = params(&s, &creator, &[2_000, 2_000, 2_000, 2_000, 1_000]);
    assert!(s
        .factory
        .try_create_basket(&creator, &salt(&s.env, 1), &bad)
        .is_err());
    assert_eq!(s.factory.basket_count(), 0);
    let addr = s.factory.basket_address(&creator, &salt(&s.env, 1));
    assert!(!s.factory.is_basket(&addr));
}

#[test]
fn wasm_update_is_admin_only() {
    let s = setup();
    let hash = s.factory.basket_wasm();
    s.factory.set_basket_wasm(&hash);
    assert_eq!(s.env.auths()[0].0, s.admin);
    assert_eq!(s.factory.admin(), s.admin);
}

/// End-to-end on the deployed WASM: 5-asset basket at 20% each, price drift,
/// arbitrageurs restore the targets, holders redeem every leg.
#[test]
fn factory_basket_rebalances_back_to_20pct() {
    let s = setup();
    let creator = Address::generate(&s.env);
    let arb = Address::generate(&s.env);
    let user = Address::generate(&s.env);
    let basket = IndexBasketClient::new(
        &s.env,
        &s.factory.create_basket(
            &creator,
            &salt(&s.env, 9),
            &params(&s, &creator, &[2_000; 5]),
        ),
    );
    basket.set_arbitrageur(&arb, &true);

    let amount = 500 * SHARE;
    let deposit = basket.quote_issue(&amount);
    for (t, a) in s.tokens.iter().zip(deposit.iter()) {
        t.mint(&user, &a);
    }
    let mut max_in = Vec::new(&s.env);
    for a in deposit.iter() {
        max_in.push_back(a);
    }
    assert_eq!(basket.issue(&user, &amount, &max_in), deposit);

    // Drift: token 0 +80%, token 3 -40%.
    s.oracle
        .set_price(&s.tokens[0].address, &(PRICES[0] * 18 / 10));
    s.oracle
        .set_price(&s.tokens[3].address, &(PRICES[3] * 6 / 10));
    assert!(basket.weights().iter().any(|w| w.abs_diff(2_000) > TOL));

    for _ in 0..20 {
        let w: StdVec<i128> = basket.weights().iter().map(|w| w as i128 - 2_000).collect();
        if w.iter().all(|d| d.abs() <= TOL as i128) {
            break;
        }
        let o = (0..5).max_by_key(|i| w[*i]).unwrap();
        let u = (0..5).min_by_key(|i| w[*i]).unwrap();
        let gap = w[o].min(-w[u]);
        let value = basket.nav() * gap / BPS as i128 * (BPS - INCENTIVE - 5) as i128 / BPS as i128;
        let price = s
            .oracle
            .lastprice(&Asset::Stellar(s.tokens[u].address.clone()))
            .unwrap()
            .price;
        let amount_in = value * 10i128.pow(DECIMALS[u]) / price;
        s.tokens[u].mint(&arb, &amount_in);
        basket.rebalance(
            &arb,
            &s.tokens[u].address,
            &s.tokens[o].address,
            &amount_in,
            &0,
        );
    }
    for w in basket.weights().iter() {
        assert!(w.abs_diff(2_000) <= TOL, "weight {w}");
    }

    let out = basket.redeem(&user, &amount, &Vec::from_array(&s.env, [0i128; 5]));
    for (t, o) in s.tokens.iter().zip(out.iter()) {
        assert!(o > 0);
        assert_eq!(t.balance(&user), o);
        assert_eq!(t.balance(&basket.address), 0);
    }
    assert_eq!(basket.total_supply(), 0);
}
