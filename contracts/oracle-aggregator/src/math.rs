//! Pure integer math for the oracle aggregator.
//!
//! All calculations are performed in fixed-point integer arithmetic to avoid
//! floating-point error and arithmetic overflow. Prices are `u128` integer
//! scaled units and deviation thresholds are expressed in basis points
//! (`1_0000` = 100%).

extern crate alloc;

use alloc::vec::Vec;

/// Basis points scale: `10_000` bps == 100%.
pub const BPS: u128 = 10_000;

/// Fractional scale used internally for fixed-point median averaging.
pub const SCALE: u128 = 1_000_000_000;

/// Absolute difference between two `u128` values without underflow.
pub fn abs_diff(a: u128, b: u128) -> u128 {
    a.saturating_sub(b).max(b.saturating_sub(a))
}

/// Relative deviation of `value` from `reference`, expressed in basis points.
///
/// Returns [`u128::MAX`] when `reference` is zero so that any non-zero value
/// is always classified as an outlier against a zero baseline. Uses
/// [`u128::saturating_mul`] so that extreme inputs degrade to "maximum
/// deviation" instead of overflowing.
pub fn deviation_bps(value: u128, reference: u128) -> u128 {
    if reference == 0 {
        if value == 0 {
            return 0;
        }
        return u128::MAX;
    }
    let diff = abs_diff(value, reference);
    diff.saturating_mul(BPS) / reference
}

/// Whether `value` deviates from `reference` by more than `threshold_bps`.
pub fn is_outlier(value: u128, reference: u128, threshold_bps: u128) -> bool {
    deviation_bps(value, reference) > threshold_bps
}

/// Overflow-safe integer midpoint: `(a + b) / 2` without overflowing `u128`.
pub fn mid(a: u128, b: u128) -> u128 {
    if a <= b {
        a + (b - a) / 2
    } else {
        b + (a - b) / 2
    }
}

/// Median of a sorted slice.
///
/// For an even number of elements the two middle values are averaged using
/// overflow-safe integer math. Returns `None` for an empty slice.
pub fn median_of_sorted(sorted: &[u128]) -> Option<u128> {
    let n = sorted.len();
    if n == 0 {
        return None;
    }
    let mid_idx = n / 2;
    if n % 2 == 1 {
        Some(sorted[mid_idx])
    } else {
        Some(mid(sorted[mid_idx - 1], sorted[mid_idx]))
    }
}

/// Median of an unsorted set of prices.
pub fn median(prices: &[u128]) -> Option<u128> {
    if prices.is_empty() {
        return None;
    }
    let mut sorted: Vec<u128> = prices.to_vec();
    sorted.sort_unstable();
    median_of_sorted(&sorted)
}

/// Iteratively trims outlier prices and returns the median of the survivors.
///
/// The algorithm repeatedly computes the median of the current set, drops any
/// value whose deviation from that median exceeds `threshold_bps`, and stops
/// when the set is stable, empty, or reduced to a single element. This keeps
/// the aggregate robust against both single spikes and clustered malicious
/// feeds while still converging to the honest median.
pub fn trimmed_median(prices: &[u128], threshold_bps: u128) -> Option<u128> {
    if prices.is_empty() {
        return None;
    }
    if prices.len() == 1 || threshold_bps >= BPS {
        return median(prices);
    }

    let mut current: Vec<u128> = prices.to_vec();
    for _ in 0..current.len().max(1) {
        let med = median(&current)?;
        let next: Vec<u128> = current
            .iter()
            .copied()
            .filter(|value| !is_outlier(*value, med, threshold_bps))
            .collect();
        if next.len() == current.len() {
            break;
        }
        current = next;
        if current.len() <= 1 {
            break;
        }
    }
    median(&current)
}

/// Whether a data point with `timestamp` (unix seconds) is stale relative to
/// `now` given the configured `heartbeat_secs` freshness window.
pub fn is_stale(timestamp: u64, now: u64, heartbeat_secs: u64) -> bool {
    heartbeat_secs == 0 || now.saturating_sub(timestamp) > heartbeat_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deviation_bps_basic() {
        assert_eq!(deviation_bps(100, 100), 0);
        assert_eq!(deviation_bps(110, 100), 1000);
        assert_eq!(deviation_bps(90, 100), 1000);
        assert_eq!(deviation_bps(600, 100), 50_000);
        assert_eq!(deviation_bps(0, 0), 0);
        assert_eq!(deviation_bps(1, 0), u128::MAX);
    }

    #[test]
    fn is_outlier_thresholds() {
        assert!(!is_outlier(105, 100, 500));
        assert!(is_outlier(110, 100, 500));
        assert!(is_outlier(600, 100, 10_000));
    }

    #[test]
    fn median_odd_and_even() {
        assert_eq!(median(&[7, 3, 5]), Some(5));
        assert_eq!(median(&[1, 2]), Some(1));
        assert_eq!(median(&[2, 2]), Some(2));
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[42]), Some(42));
    }

    #[test]
    fn median_even_overflow_safe() {
        let big = u128::MAX;
        assert_eq!(median(&[big, big]), Some(big));
        assert_eq!(median(&[big - 1, big]), Some(big - 1));
    }

    #[test]
    fn trimmed_median_removes_spike() {
        // Five feeds, one spiked +500%.
        let prices = [100, 100, 101, 99, 600];
        assert_eq!(trimmed_median(&prices, 500), Some(100));
    }

    #[test]
    fn trimmed_median_removes_clustered_outliers() {
        // Three honest feeds clustered around 100 and two colluding feeds at 1000.
        let prices = [100, 101, 99, 1000, 1000];
        assert_eq!(trimmed_median(&prices, 500), Some(100));
    }

    #[test]
    fn trimmed_median_single_and_empty() {
        assert_eq!(trimmed_median(&[42], 500), Some(42));
        assert_eq!(trimmed_median(&[], 500), None);
    }

    #[test]
    fn is_stale_windows() {
        assert!(!is_stale(1000, 1000, 300));
        assert!(!is_stale(1000, 1299, 300));
        assert!(is_stale(1000, 1301, 300));
        assert!(is_stale(1000, 2000, 300));
        // A zero heartbeat window means no freshness allowance (always stale).
        assert!(is_stale(0, 0, 0));
        assert!(is_stale(5, 0, 0));
    }
}
