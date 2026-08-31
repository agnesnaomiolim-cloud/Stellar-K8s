//! Property-based tests verifying the mathematical safety of the oracle
//! aggregator's median, outlier-trimming, deviation, and staleness logic
//! across edge-case input vectors.
//!
//! These tests deliberately exercise extreme values (`u128::MAX`, zero, and
//! clustered adversarial feeds) to prove the arithmetic never overflows,
//! panics, or produces a result outside the honest range.

use proptest::prelude::*;
use soroban_sdk::{Env, Address, Symbol};
use soroban_sdk::testutils::{Address as _, Ledger as _, Register as _};

// The aggregator's pure math is re-exported through the crate root for reuse.
use oracle_aggregator::math::{
    trimmed_median, median, median_of_sorted, deviation_bps, is_outlier, is_stale, mid, BPS,
};

proptest! {
    #[test]
    fn trimmed_median_is_within_honest_range(
        values in prop::collection::vec(1u128..1_000_000_000u128, 1..=9),
        threshold in 1u128..=BPS,
    ) {
        let result = trimmed_median(&values, threshold);
        if let Some(med) = result {
            prop_assert!(med <= *values.iter().max().unwrap());
            prop_assert!(med >= *values.iter().min().unwrap());
        } else {
            // When trimming removes everything (deeply inconsistent data) the
            // function degrades to "no price" rather than panicking.
            prop_assert!(threshold < BPS);
        }
    }

    #[test]
    fn trimmed_median_resists_single_spike(
        count in 4usize..=8,
        honest in 1_000u128..10_000u128,
        threshold in 200u128..=BPS,
    ) {
        // A tight honest cluster plus a single 5x spike. The aggregate must be
        // dominated by the honest cluster.
        let mut prices = vec![honest; count];
        prices.push(honest * 5);
        let result = trimmed_median(&prices, threshold).unwrap();
        prop_assert!(result < honest * 2, "median {} should ignore spike", result);
    }

    #[test]
    fn median_identity_for_single_and_even(values in prop::collection::vec(0u128..u128::MAX, 1..=2)) {
        if values.len() == 1 {
            prop_assert_eq!(median(&values), Some(values[0]));
        } else if values.len() == 2 {
            // Median of two values must equal the overflow-safe midpoint.
            let mut sorted = values.clone();
            sorted.sort_unstable();
            prop_assert_eq!(median(&values), Some(mid(sorted[0], sorted[1])));
        }
    }

    #[test]
    fn median_of_sorted_is_middle_for_odd(
        mut values in prop::collection::vec(0u128..u128::MAX, 1..=15),
    ) {
        values.sort_unstable();
        if values.len() % 2 == 1 {
            let idx = values.len() / 2;
            prop_assert_eq!(median_of_sorted(&values), Some(values[idx]));
        }
    }

    #[test]
    fn deviation_bps_is_bounded_for_nonzero_reference(
        a in 1u128..1_000_000_000u128,
        b in 1u128..1_000_000_000u128,
    ) {
        // Relative deviation is always representable without overflow and is
        // zero exactly when the value equals the reference.
        let d = deviation_bps(a, b);
        if a == b {
            prop_assert_eq!(d, 0);
        } else {
            prop_assert!(d > 0);
        }
    }

    #[test]
    fn outlier_detection_agrees_with_threshold(
        value in 1u128..1_000_000_000u128,
        reference in 1u128..1_000_000_000u128,
        threshold in 1u128..=BPS,
    ) {
        let is_out = is_outlier(value, reference, threshold);
        prop_assert_eq!(is_out, deviation_bps(value, reference) > threshold);
        if value == reference {
            prop_assert!(!is_out);
        }
    }

    #[test]
    fn staleness_boundary(
        timestamp in 0u64..u64::MAX,
        now in 0u64..u64::MAX,
        heartbeat in 1u64..100_000u64,
    ) {
        let stale = is_stale(timestamp, now, heartbeat);
        // Within the window is fresh; exactly at the boundary is fresh.
        if timestamp <= now && now.saturating_sub(timestamp) <= heartbeat {
            prop_assert!(!stale);
        }
        if timestamp <= now && now.saturating_sub(timestamp) > heartbeat {
            prop_assert!(stale);
        }
    }

    #[test]
    fn median_is_overflow_safe_at_extremes(
        a in prop::num::u128::ANY,
        b in prop::num::u128::ANY,
    ) {
        // The midpoint must never overflow and must lie between the inputs.
        let m = mid(a, b);
        prop_assert!(m >= a.min(b));
        prop_assert!(m <= a.max(b));
        if a == b {
            prop_assert_eq!(m, a);
        }
    }
}

/// End-to-end contract behaviour with a simulated price spike: 1 of 5 oracles
/// reports a +500% deviation and the aggregated price stays stable.
#[test]
fn contract_aggregation_ignores_fivefold_spike() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let contract_id = oracle_aggregator::OracleAggregator.register(&env, None, ());
    let client = oracle_aggregator::OracleAggregatorClient::new(&env, &contract_id);

    client.initialize(
        &admin,
        &Some(500u128), // 5% threshold
        &Some(300u64),  // 5 minute heartbeat
        &Some(1u32),
    );

    let asset = Symbol::new(&env, "ETHUSDT");

    // 4 honest feeds report ~100.0 (scaled to 1e6 units for 4 decimals).
    let honest_price = 100_0000u128;
    let feeds: Vec<Address> = (0..4).map(|_| Address::generate(&env)).collect();
    for feed in &feeds {
        client.add_feed(feed);
        client.submit_price(feed, &asset, &honest_price, &1_000_000_000u64);
    }

    // 1 malicious feed reports +500% (5x the honest price).
    let spike_feed = Address::generate(&env);
    client.add_feed(&spike_feed);
    client.submit_price(
        &spike_feed,
        &asset,
        &(honest_price * 5),
        &1_000_000_000u64,
    );

    // The aggregated price must remain at the honest median, not the spike.
    let price = client.get_price(&asset);
    assert_eq!(price, honest_price);
}

/// Stale price pushes older than the heartbeat must be rejected at write time.
#[test]
fn contract_rejects_stale_pushes() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000_000);

    let admin = Address::generate(&env);
    let contract_id = oracle_aggregator::OracleAggregator.register(&env, None, ());
    let client = oracle_aggregator::OracleAggregatorClient::new(&env, &contract_id);

    client.initialize(&admin, &Some(500u128), &Some(300u64), &Some(1u32));

    let feed = Address::generate(&env);
    client.add_feed(&feed);

    let asset = Symbol::new(&env, "BTCUSDT");
    let now = env.ledger().timestamp();
    let result = client.try_submit_price(&feed, &asset, &100_0000u128, &(now - 400));
    assert_eq!(
        result,
        Err(Ok(oracle_aggregator::Error::StaleData))
    );
}

/// Unauthorized feeds must be rejected when pushing prices.
#[test]
fn contract_rejects_unauthorized_feed() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000_000);

    let admin = Address::generate(&env);
    let contract_id = oracle_aggregator::OracleAggregator.register(&env, None, ());
    let client = oracle_aggregator::OracleAggregatorClient::new(&env, &contract_id);

    client.initialize(&admin, &Some(500u128), &Some(300u64), &Some(1u32));

    let rogue = Address::generate(&env);
    let asset = Symbol::new(&env, "BTCUSDT");
    let now = env.ledger().timestamp();
    let result = client.try_submit_price(&rogue, &asset, &100_0000u128, &now);
    assert_eq!(
        result,
        Err(Ok(oracle_aggregator::Error::FeedNotAuthorized))
    );
}

/// Stale prices are ignored at read time even when previously accepted.
#[test]
fn contract_ignores_stale_data_at_read_time() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000_000);

    let admin = Address::generate(&env);
    let contract_id = oracle_aggregator::OracleAggregator.register(&env, None, ());
    let client = oracle_aggregator::OracleAggregatorClient::new(&env, &contract_id);

    client.initialize(&admin, &Some(500u128), &Some(300u64), &Some(2u32));

    let asset = Symbol::new(&env, "XLMUSDT");
    let feed_a = Address::generate(&env);
    let feed_b = Address::generate(&env);
    client.add_feed(&feed_a);
    client.add_feed(&feed_b);

    let now = env.ledger().timestamp();
    // Both pushes are within the heartbeat window when submitted...
    client.submit_price(&feed_a, &asset, &100_0000u128, &(now - 100));
    client.submit_price(&feed_b, &asset, &101_0000u128, &now);

    // ...but by read time feed_a has aged past the heartbeat.
    env.ledger().set_timestamp(now + 250);

    // Only feed_b is fresh; with min_feeds = 2 the aggregation must fail.
    let result = client.try_get_price(&asset);
    assert_eq!(
        result,
        Err(Ok(oracle_aggregator::Error::NotEnoughFeeds))
    );
}
