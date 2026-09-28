extern crate std;

use num_bigint::BigUint;
use proptest::prelude::*;
use soroban_sdk::{
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env,
};
use std::vec::Vec;

use crate::math::*;
use crate::ticks::*;
use crate::{ClAmm, ClAmmClient, Error};

// ======================================================================
// Fixed-point math, checked against an arbitrary-precision reference
// ======================================================================

fn big(x: u128) -> BigUint {
    BigUint::from(x)
}

fn ceil_div(n: &BigUint, d: &BigUint) -> BigUint {
    (n + d - BigUint::from(1u8)) / d
}

/// Reference: Δx = L·Q64·(sb − sa) / (sa·sb), exactly.
fn ref_amount0(sa: u128, sb: u128, l: u128, up: bool) -> BigUint {
    let (sa, sb) = if sa > sb { (sb, sa) } else { (sa, sb) };
    let n = big(l) * big(Q64) * big(sb - sa);
    let d = big(sa) * big(sb);
    if up {
        ceil_div(&n, &d)
    } else {
        n / d
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    #[test]
    fn mul_div_matches_bigint(a in any::<u128>(), b in any::<u128>(), d in 1..=u128::MAX) {
        let exact = big(a) * big(b);
        let q = &exact / big(d);
        match mul_div_rem(a, b, d) {
            Some((got_q, got_r)) => {
                prop_assert_eq!(big(got_q), q);
                prop_assert_eq!(big(got_r), exact % big(d));
                let up = mul_div_up(a, b, d);
                let ref_up = ceil_div(&(big(a) * big(b)), &big(d));
                if ref_up <= big(u128::MAX) {
                    prop_assert_eq!(up.map(big), Some(ref_up));
                }
            }
            None => prop_assert!(q > big(u128::MAX)),
        }
    }

    #[test]
    fn mul_div_small_divisors(a in any::<u128>(), b in any::<u128>(), d in 1u128..=u64::MAX as u128) {
        let q = big(a) * big(b) / big(d);
        prop_assert_eq!(mul_div(a, b, d).map(big), (q <= big(u128::MAX)).then_some(q));
    }

    #[test]
    fn amount0_delta_is_exact(
        t_a in MIN_TICK..=MAX_TICK,
        t_b in MIN_TICK..=MAX_TICK,
        l in 0u128..=1u128 << 100,
        up in any::<bool>(),
    ) {
        let (sa, sb) = (sqrt_price_at_tick(t_a), sqrt_price_at_tick(t_b));
        let reference = ref_amount0(sa, sb, l, up);
        match amount0_delta(sa, sb, l, up) {
            Some(got) => prop_assert_eq!(big(got), reference),
            None => prop_assert!(reference > big(u128::MAX)),
        }
    }

    #[test]
    fn amount1_delta_is_exact(
        t_a in MIN_TICK..=MAX_TICK,
        t_b in MIN_TICK..=MAX_TICK,
        l in 0u128..=1u128 << 100,
        up in any::<bool>(),
    ) {
        let (sa, sb) = (sqrt_price_at_tick(t_a), sqrt_price_at_tick(t_b));
        let (lo, hi) = if sa > sb { (sb, sa) } else { (sa, sb) };
        let n = big(l) * big(hi - lo);
        let reference = if up { ceil_div(&n, &big(Q64)) } else { n / big(Q64) };
        match amount1_delta(sa, sb, l, up) {
            Some(got) => prop_assert_eq!(big(got), reference),
            None => prop_assert!(reference > big(u128::MAX)),
        }
    }

    /// The swap step never charges more than supplied, never leaves its
    /// segment, and always rounds in the pool's favour.
    #[test]
    fn swap_step_invariants(
        t_cur in -200_000i32..200_000,
        dt in -20_000i32..20_000,
        l in 1u128..=1u128 << 90,
        remaining in 1u128..=1u128 << 100,
        fee in prop::sample::select(std::vec![0u32, 100, 500, 3000, 10_000, 100_000]),
    ) {
        let cur = sqrt_price_at_tick(t_cur);
        let target = sqrt_price_at_tick(t_cur + dt);
        let zfo = cur >= target;
        let s = compute_swap_step(cur, target, l, remaining, fee).unwrap();

        prop_assert!(s.amount_in + s.fee_amount <= remaining);
        if zfo {
            prop_assert!(s.sqrt_price_next <= cur && s.sqrt_price_next >= target);
        } else {
            prop_assert!(s.sqrt_price_next >= cur && s.sqrt_price_next <= target);
        }
        if s.sqrt_price_next != target {
            prop_assert_eq!(s.amount_in + s.fee_amount, remaining);
        }
        // Input covers the exact (ceil) cost of the move; output never
        // exceeds the exact (floor) value released by it.
        let (lo, hi) = if zfo { (s.sqrt_price_next, cur) } else { (cur, s.sqrt_price_next) };
        let (need_in, max_out) = if zfo {
            (amount0_delta(lo, hi, l, true).unwrap(), amount1_delta(lo, hi, l, false).unwrap())
        } else {
            (amount1_delta(lo, hi, l, true).unwrap(), amount0_delta(lo, hi, l, false).unwrap())
        };
        prop_assert!(s.amount_in >= need_in);
        prop_assert!(s.amount_out <= max_out);
        // Fee is at least the configured rate on the consumed input.
        prop_assert!(big(s.fee_amount) * big(FEE_DENOMINATOR - fee as u128) >= big(s.amount_in) * big(fee as u128));
    }

    #[test]
    fn tick_roundtrip(t in MIN_TICK..MAX_TICK) {
        let p = sqrt_price_at_tick(t);
        let next = sqrt_price_at_tick(t + 1);
        prop_assert_eq!(tick_at_sqrt_price(p), t);
        prop_assert_eq!(tick_at_sqrt_price(next - 1), t);
        prop_assert_eq!(tick_at_sqrt_price(p + (next - p) / 2), t);
    }

    #[test]
    fn sqrt_price_accuracy(t in MIN_TICK..=MAX_TICK) {
        // 2·ln(√P) must equal t·ln(1.0001); the only material error is the
        // final Q64.64 rounding (≤ 1 ulp ⇒ relative ≤ 1/√P).
        let p = sqrt_price_at_tick(t) as f64 / Q64 as f64;
        let err = (2.0 * p.ln() - t as f64 * 1.0001f64.ln()).abs();
        prop_assert!(err < 1e-9, "tick {} err {}", t, err);
    }

    #[test]
    fn next_bit_matches_scan(word in any::<u128>(), bit in 0u32..128, lte in any::<bool>()) {
        let expect = if lte {
            (0..=bit).rev().find(|b| word & (1 << b) != 0).map(|b| (b, true)).unwrap_or((0, false))
        } else {
            (bit..128).find(|b| word & (1 << b) != 0).map(|b| (b, true)).unwrap_or((127, false))
        };
        prop_assert_eq!(next_bit_in_word(word, bit, lte), expect);
    }
}

#[test]
fn sqrt_price_is_strictly_monotone_over_entire_domain() {
    let mut prev = sqrt_price_at_tick(MIN_TICK);
    for t in MIN_TICK + 1..=MAX_TICK {
        let p = sqrt_price_at_tick(t);
        assert!(p > prev, "not increasing at tick {t}");
        prev = p;
    }
}

#[test]
fn domain_constants() {
    assert_eq!(sqrt_price_at_tick(0), Q64);
    assert_eq!(MAX_TICK, -MIN_TICK);
    // √P spans roughly [2^-32, 2^32] ⇒ Q64.64 values within [2^31, 2^97).
    const _: () = assert!(MIN_SQRT_PRICE >= 1 << 31 && MIN_SQRT_PRICE < 1 << 33);
    const _: () = assert!(MAX_SQRT_PRICE >= 1 << 95 && MAX_SQRT_PRICE < 1 << 97);
    assert_eq!(tick_at_sqrt_price(MIN_SQRT_PRICE), MIN_TICK);
    assert_eq!(tick_at_sqrt_price(MAX_SQRT_PRICE), MAX_TICK);
    // Summing the per-tick cap over every usable tick cannot overflow u128.
    for s in [1, 10, 60, 200, MAX_TICK_SPACING] {
        let n = ((MAX_TICK / s) - (MIN_TICK / s)) as u128 + 1;
        assert!(max_liquidity_per_tick(s).checked_mul(n).is_some());
    }
}

#[test]
fn liquidity_for_amounts_never_overspends() {
    let (sa, sb) = (sqrt_price_at_tick(-600), sqrt_price_at_tick(900));
    for p in [sqrt_price_at_tick(-1000), Q64, sqrt_price_at_tick(1200)] {
        let l = liquidity_for_amounts(p, sa, sb, 1_000_000_000, 2_000_000_000).unwrap();
        let cp = p.clamp(sa, sb);
        assert!(amount0_delta(cp, sb, l, true).unwrap() <= 1_000_000_000);
        assert!(amount1_delta(sa, cp, l, true).unwrap() <= 2_000_000_000);
    }
}

// ======================================================================
// Contract-level tests
// ======================================================================

const BIG: i128 = 1 << 100;

struct Pool {
    env: Env,
    id: Address,
    t0: Address,
    t1: Address,
}

impl Pool {
    fn new(fee: u32, spacing: i32, sqrt_price: u128) -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.cost_estimate().budget().reset_unlimited();
        let admin = Address::generate(&env);
        let t0 = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let t1 = env.register_stellar_asset_contract_v2(admin).address();
        let id = env.register(ClAmm, ());
        ClAmmClient::new(&env, &id).initialize(&t0, &t1, &fee, &spacing, &sqrt_price);
        Pool { env, id, t0, t1 }
    }

    fn amm(&self) -> ClAmmClient<'_> {
        ClAmmClient::new(&self.env, &self.id)
    }

    fn user(&self) -> Address {
        let u = Address::generate(&self.env);
        StellarAssetClient::new(&self.env, &self.t0).mint(&u, &BIG);
        StellarAssetClient::new(&self.env, &self.t1).mint(&u, &BIG);
        u
    }

    fn balances(&self, who: &Address) -> (i128, i128) {
        (
            TokenClient::new(&self.env, &self.t0).balance(who),
            TokenClient::new(&self.env, &self.t1).balance(who),
        )
    }

    fn mint(&self, lp: &Address, lo: i32, hi: i32, l: u128) -> (i128, i128) {
        self.amm().mint(lp, &lo, &hi, &l, &i128::MAX, &i128::MAX)
    }

    /// Burns everything and collects; returns (principal, total collected).
    fn exit(&self, lp: &Address, lo: i32, hi: i32) -> ((i128, i128), (i128, i128)) {
        let l = self.amm().position(lp, &lo, &hi).liquidity;
        let principal = self.amm().burn(lp, &lo, &hi, &l);
        let got = self.amm().collect(lp, &lo, &hi, &i128::MAX, &i128::MAX);
        (principal, got)
    }
}

#[test]
fn initialize_validates_parameters() {
    let p = Pool::new(3000, 10, Q64);
    let a = p.amm();
    assert_eq!(
        a.try_initialize(&p.t0, &p.t1, &3000, &10, &Q64),
        Err(Ok(Error::AlreadyInitialized))
    );

    let env = Env::default();
    let id = env.register(ClAmm, ());
    let c = ClAmmClient::new(&env, &id);
    let (x, y) = (Address::generate(&env), Address::generate(&env));
    assert_eq!(
        c.try_initialize(&x, &x, &3000, &10, &Q64),
        Err(Ok(Error::IdenticalTokens))
    );
    assert_eq!(
        c.try_initialize(&x, &y, &100_001, &10, &Q64),
        Err(Ok(Error::InvalidFee))
    );
    assert_eq!(
        c.try_initialize(&x, &y, &3000, &0, &Q64),
        Err(Ok(Error::InvalidTickSpacing))
    );
    assert_eq!(
        c.try_initialize(&x, &y, &3000, &10, &MAX_SQRT_PRICE),
        Err(Ok(Error::InvalidSqrtPrice))
    );
    assert_eq!(
        c.try_initialize(&x, &y, &3000, &10, &(MIN_SQRT_PRICE - 1)),
        Err(Ok(Error::InvalidSqrtPrice))
    );
}

#[test]
fn mint_amounts_depend_on_range_position() {
    let p = Pool::new(3000, 10, Q64); // tick 0
    let lp = p.user();
    let (a0, a1) = p.mint(&lp, 100, 200, 1_000_000_000);
    assert!(a0 > 0 && a1 == 0, "range above price holds only token0");
    let (a0, a1) = p.mint(&lp, -200, -100, 1_000_000_000);
    assert!(a0 == 0 && a1 > 0, "range below price holds only token1");
    let (a0, a1) = p.mint(&lp, -100, 100, 1_000_000_000);
    assert!(a0 > 0 && a1 > 0);
    assert_eq!(
        p.amm().pool().liquidity,
        1_000_000_000,
        "only the in-range position is active"
    );

    let a = p.amm();
    assert_eq!(
        a.try_mint(&lp, &-5, &100, &1, &BIG, &BIG),
        Err(Ok(Error::InvalidTickRange))
    );
    assert_eq!(
        a.try_mint(&lp, &100, &100, &1, &BIG, &BIG),
        Err(Ok(Error::InvalidTickRange))
    );
    assert_eq!(
        a.try_mint(&lp, &-100, &100, &0, &BIG, &BIG),
        Err(Ok(Error::InvalidAmount))
    );
    assert_eq!(
        a.try_mint(&lp, &-100, &100, &1_000_000, &0, &0),
        Err(Ok(Error::SlippageExceeded))
    );
    assert_eq!(
        a.try_burn(&lp, &-100, &100, &2_000_000_000),
        Err(Ok(Error::InsufficientPositionLiquidity))
    );
}

#[test]
fn mint_burn_roundtrip_never_returns_more_than_deposited() {
    let p = Pool::new(3000, 1, sqrt_price_at_tick(37) + 12345);
    let lp = p.user();
    for (lo, hi, l) in [
        (-50, 80, 7_777_777u128),
        (40, 90, 1),
        (-3, 38, 123_456_789_012),
    ] {
        let (d0, d1) = p.mint(&lp, lo, hi, l);
        let ((w0, w1), (c0, c1)) = p.exit(&lp, lo, hi);
        assert!(w0 <= d0 && w1 <= d1 && d0 - w0 <= 1 && d1 - w1 <= 1);
        assert_eq!((c0, c1), (w0, w1));
        assert_eq!(p.amm().position(&lp, &lo, &hi), crate::Position::default());
    }
    assert_eq!(p.amm().pool().liquidity, 0);
    assert_eq!(
        p.amm().tick(&-50),
        TickInfo::default(),
        "emptied ticks are deleted"
    );
}

#[test]
fn fees_accrue_only_to_ranges_that_are_crossed() {
    let p = Pool::new(3000, 10, Q64);
    let (active, idle, trader) = (p.user(), p.user(), p.user());
    p.mint(&active, -1000, 1000, 10u128.pow(12));
    p.mint(&idle, 5000, 6000, 10u128.pow(12));

    let r = p.amm().swap(&trader, &true, &1_000_000, &0, &0);
    assert_eq!(r.amount_in, 1_000_000);
    let fee = 1_000_000u128 * 3000 / 1_000_000; // exact here: input divisible

    p.amm().burn(&active, &-1000, &1000, &0);
    p.amm().burn(&idle, &5000, &6000, &0);
    let owed_active = p.amm().position(&active, &-1000, &1000).tokens_owed_0;
    let owed_idle = p.amm().position(&idle, &5000, &6000);
    assert!(
        owed_active <= fee && fee - owed_active <= 1,
        "fee {fee} owed {owed_active}"
    );
    assert_eq!((owed_idle.tokens_owed_0, owed_idle.tokens_owed_1), (0, 0));
}

#[test]
fn fees_split_pro_rata_between_overlapping_positions() {
    let p = Pool::new(10_000, 10, Q64);
    let (a, b, trader) = (p.user(), p.user(), p.user());
    p.mint(&a, -500, 500, 1_000_000_000_000);
    p.mint(&b, -500, 500, 3_000_000_000_000);
    p.amm().swap(&trader, &false, &50_000_000, &0, &0);
    p.amm().burn(&a, &-500, &500, &0);
    p.amm().burn(&b, &-500, &500, &0);
    let fa = p.amm().position(&a, &-500, &500).tokens_owed_1;
    let fb = p.amm().position(&b, &-500, &500).tokens_owed_1;
    // 1% of 50M = 500k split 1:3.
    assert!(125_000 - fa <= 1 && 375_000 - fb <= 1, "{fa} {fb}");
}

#[test]
fn swap_respects_price_limit_and_slippage() {
    let p = Pool::new(3000, 10, Q64);
    let (lp, trader) = (p.user(), p.user());
    p.mint(&lp, -1000, 1000, 10u128.pow(10));
    let limit = sqrt_price_at_tick(-50);
    let r = p.amm().swap(&trader, &true, &BIG, &limit, &0);
    assert_eq!(r.sqrt_price, limit);
    assert!(r.amount_in < BIG, "only consumed input is charged");
    assert_eq!(p.balances(&trader).0, BIG - r.amount_in);

    let a = p.amm();
    assert_eq!(
        a.try_swap(&trader, &true, &1000, &(limit + 1), &0),
        Err(Ok(Error::InvalidPriceLimit))
    );
    assert_eq!(
        a.try_swap(&trader, &false, &1000, &0, &i128::MAX),
        Err(Ok(Error::SlippageExceeded))
    );
    assert_eq!(
        a.try_swap(&trader, &false, &0, &0, &0),
        Err(Ok(Error::InvalidAmount))
    );
}

/// A single market order sweeping 100 stacked LP bins: every bin is crossed,
/// liquidity is re-derived at each boundary and all LPs remain solvent.
#[test]
fn massive_order_crosses_many_bins() {
    let p = Pool::new(3000, 10, Q64);
    let trader = p.user();
    let mut lps = Vec::new();
    for i in 0..100 {
        let lp = p.user();
        let lo = -10 * (i + 1);
        p.mint(&lp, lo, lo + 10, 1_000_000_000_000);
        lps.push((lp, lo));
    }
    let r = p.amm().swap(&trader, &true, &(1i128 << 80), &0, &0);
    let pool = p.amm().pool();
    assert!(
        pool.tick < -1000,
        "price swept past every bin: {}",
        pool.tick
    );
    assert_eq!(pool.liquidity, 0);
    assert!(r.amount_out > 0);

    let mut paid = (0i128, 0i128);
    for (lp, lo) in &lps {
        let ((w0, w1), (c0, c1)) = p.exit(lp, *lo, lo + 10);
        assert!(c0 >= w0 && c1 == w1, "every crossed bin earned token0 fees");
        assert!(c0 > w0);
        paid = (paid.0 + c0, paid.1 + c1);
    }
    let dust = p.balances(&p.id);
    assert!(
        dust.0 >= 0 && dust.0 <= 400 && dust.1 >= 0 && dust.1 <= 400,
        "dust {dust:?}"
    );
}

/// Executes the contract compiled to WASM (when built) to check that a
/// multi-bin order fits in the network CPU budget under real metering.
#[test]
fn wasm_multi_tick_swap_fits_budget() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/target/wasm32v1-none/release/cl_amm.wasm"
    );
    let Ok(wasm) = std::fs::read(path) else {
        std::eprintln!("skipping: build with `cargo build --target wasm32v1-none --release` first");
        return;
    };
    let env = Env::default();
    env.mock_all_auths();
    env.cost_estimate().budget().reset_unlimited();
    let admin = Address::generate(&env);
    let t0 = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let t1 = env.register_stellar_asset_contract_v2(admin).address();
    let id = env.register(wasm.as_slice(), ());
    let amm = ClAmmClient::new(&env, &id);
    amm.initialize(&t0, &t1, &3000, &10, &Q64);
    let user = Address::generate(&env);
    StellarAssetClient::new(&env, &t0).mint(&user, &BIG);
    StellarAssetClient::new(&env, &t1).mint(&user, &BIG);
    for i in 0..40 {
        let lo = -10 * (i + 1);
        amm.mint(&user, &lo, &(lo + 10), &1_000_000_000_000, &BIG, &BIG);
    }

    env.cost_estimate().budget().reset_default();
    amm.swap(&user, &true, &(1i128 << 80), &sqrt_price_at_tick(-400), &0);
    let cpu = env.cost_estimate().budget().cpu_instruction_cost();
    std::eprintln!("40-bin swap: {cpu} CPU instructions");
    assert!(cpu < 100_000_000);
}

// ======================================================================
// Fuzzing: random LP ranges + massive multi-tick orders
// ======================================================================

#[derive(Debug, Clone)]
struct PosSpec {
    lower_off: i32,
    width: i32,
    liquidity: u128,
}

fn pos_strategy() -> impl Strategy<Value = PosSpec> {
    (-300i32..300, 1i32..150, 1_000u128..=100_000_000_000_000).prop_map(
        |(lower_off, width, liquidity)| PosSpec {
            lower_off,
            width,
            liquidity,
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// For any set of LP ranges and any sequence of (possibly enormous)
    /// orders: balances move exactly by the reported amounts, a round trip
    /// never profits, and after every LP exits the pool holds only bounded
    /// rounding dust — i.e. fees were distributed precisely and nothing
    /// underflowed.
    #[test]
    fn fuzz_multi_tick_swaps_conserve_value(
        spacing in prop::sample::select(std::vec![1i32, 10, 60]),
        fee in prop::sample::select(std::vec![0u32, 500, 3000, 10_000]),
        start_tick in -5_000i32..5_000,
        positions in prop::collection::vec(pos_strategy(), 1..6),
        swaps in prop::collection::vec((any::<bool>(), 1u128..=1u128 << 70, 0i32..20_000), 1..8),
    ) {
        let p = Pool::new(fee, spacing, sqrt_price_at_tick(start_tick) + 1);
        let trader = p.user();
        let base = start_tick.div_euclid(spacing);

        let mut lps = Vec::new();
        for spec in &positions {
            let lo = (base + spec.lower_off) * spacing;
            let hi = lo + spec.width * spacing;
            if lo < MIN_TICK || hi > MAX_TICK { continue; }
            let lp = p.user();
            p.mint(&lp, lo, hi, spec.liquidity);
            lps.push((lp, lo, hi));
        }

        let mut nswaps = 0i128;
        for (zfo, amount, reach) in swaps {
            let pool = p.amm().pool();
            let limit_tick = if zfo { pool.tick - reach } else { pool.tick + reach + 1 };
            let limit = sqrt_price_at_tick(limit_tick.clamp(MIN_TICK + 1, MAX_TICK - 1));
            let before = p.balances(&trader);
            match p.amm().try_swap(&trader, &zfo, &(amount as i128), &limit, &0) {
                Ok(Ok(r)) => {
                    nswaps += 1;
                    let after = p.balances(&trader);
                    let (d_in, d_out) = if zfo {
                        (before.0 - after.0, after.1 - before.1)
                    } else {
                        (before.1 - after.1, after.0 - before.0)
                    };
                    prop_assert_eq!((d_in, d_out), (r.amount_in, r.amount_out));
                    prop_assert!(r.amount_in <= amount as i128);

                    // Round trip: selling the proceeds straight back can
                    // never return more than was put in.
                    if r.amount_out > 0 {
                        if let Ok(Ok(back)) = p.amm().try_swap(&trader, &!zfo, &r.amount_out, &0, &0) {
                            nswaps += 1;
                            prop_assert!(back.amount_out <= r.amount_in);
                        }
                    }
                }
                Err(Ok(e)) => prop_assert_eq!(e, Error::InvalidPriceLimit),
                other => return Err(TestCaseError::fail(std::format!("unexpected {other:?}"))),
            }
        }

        for (lp, lo, hi) in &lps {
            p.exit(lp, *lo, *hi);
        }
        let pool = p.amm().pool();
        prop_assert_eq!(pool.liquidity, 0);

        // Every LP was paid in full (collect would have failed otherwise);
        // what remains is rounding dust, bounded per position and per step.
        let dust = p.balances(&p.id);
        // Only steps with active liquidity lose (< 1 unit of) fee growth to
        // flooring, and a swap has at most 2·positions + a few such steps.
        let npos = lps.len() as i128;
        let bound = 4 * npos + nswaps * (2 * npos + 4);
        prop_assert!(dust.0 >= 0 && dust.1 >= 0);
        prop_assert!(dust.0 <= bound && dust.1 <= bound, "dust {:?} bound {}", dust, bound);
    }
}
