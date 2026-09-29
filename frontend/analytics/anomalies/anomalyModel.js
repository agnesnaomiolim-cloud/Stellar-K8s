/**
 * anomalyModel.js
 *
 * Core statistical model for the Anomaly Detection Workbench (#127).
 *
 * Provides efficient, pure functions for:
 *   - Computing rolling mean and standard deviation over time-series windows.
 *   - Classifying individual data points as normal / warning / critical based on
 *     configurable sigma (σ) thresholds.
 *   - Comparing a current observation window against a historical baseline.
 *   - Generating baseline statistics from historical series.
 *
 * All computations are performed incrementally where possible to avoid
 * browser rendering stalls on large datasets.
 *
 * Anomaly severity levels:
 *   normal    within ±WARN_SIGMA σ of the mean
 *   warning   between ±WARN_SIGMA and ±CRIT_SIGMA σ
 *   critical  beyond ±CRIT_SIGMA σ (e.g. p99 latency spike)
 */

/** Default warning threshold: values beyond this many σ are "warning". */
export const WARN_SIGMA = 2;

/** Default critical threshold: values beyond this many σ are "critical". */
export const CRIT_SIGMA = 3;

/**
 * Minimum number of data points required before anomaly detection engages.
 * Prevents false positives on initial sparse data.
 */
export const MIN_WINDOW = 5;

/**
 * @typedef {'normal'|'warning'|'critical'} AnomalySeverity
 */

/**
 * @typedef {Object} MetricPoint
 * @property {string} timestamp  ISO-8601 timestamp string
 * @property {number} value      Metric value at this timestamp
 */

/**
 * @typedef {Object} AnomalyPoint
 * @property {string}         timestamp   ISO-8601 timestamp string
 * @property {number}         value       Raw metric value
 * @property {number}         mean        Rolling mean at this point
 * @property {number}         stddev      Rolling standard deviation at this point
 * @property {number}         upperWarn   mean + WARN_SIGMA * stddev
 * @property {number}         lowerWarn   mean - WARN_SIGMA * stddev
 * @property {number}         upperCrit   mean + CRIT_SIGMA * stddev
 * @property {number}         lowerCrit   mean - CRIT_SIGMA * stddev
 * @property {number}         zScore      (value - mean) / stddev, or 0 if stddev = 0
 * @property {AnomalySeverity} severity   Classification of this data point
 * @property {boolean}        isAnomaly   true when severity !== 'normal'
 */

/**
 * @typedef {Object} BaselineStats
 * @property {number} mean    Population mean of the baseline series
 * @property {number} stddev  Population standard deviation of the baseline series
 * @property {number} min     Minimum value
 * @property {number} max     Maximum value
 * @property {number} p50     Median (50th percentile)
 * @property {number} p95     95th percentile
 * @property {number} p99     99th percentile
 * @property {number} count   Number of samples in the baseline
 */

/**
 * @typedef {Object} BaselineComparison
 * @property {number}         currentMean     Mean of the current (recent) window
 * @property {number}         currentStddev   Stddev of the current window
 * @property {number}         baselineMean    Mean of the historical baseline
 * @property {number}         baselineStddev  Stddev of the historical baseline
 * @property {number}         deltaMean       currentMean - baselineMean
 * @property {number}         deltaPct        deltaMean / baselineMean * 100, or 0 if baseline mean = 0
 * @property {number}         zScore          (currentMean - baselineMean) / baselineStddev, or 0
 * @property {AnomalySeverity} severity       Classification of the comparison
 */

/**
 * Computes the mean of an array of numbers.
 * Returns 0 for empty arrays.
 *
 * @param {number[]} values
 * @returns {number}
 */
export function mean(values) {
  if (values.length === 0) return 0;
  return values.reduce((s, v) => s + v, 0) / values.length;
}

/**
 * Computes the population standard deviation of an array of numbers.
 * Returns 0 for arrays with fewer than 2 elements.
 *
 * Using population (not sample) stddev intentionally: the window represents
 * all observed values, not a sample of a larger population.
 *
 * @param {number[]} values
 * @param {number}   [precomputedMean]  Optional pre-computed mean for efficiency
 * @returns {number}
 */
export function stddev(values, precomputedMean) {
  if (values.length < 2) return 0;
  const m = precomputedMean ?? mean(values);
  const variance = values.reduce((s, v) => s + (v - m) ** 2, 0) / values.length;
  return Math.sqrt(variance);
}

/**
 * Computes the p-th percentile of an array (linear interpolation, R-7 method).
 *
 * @param {number[]} sortedValues  Values sorted ascending
 * @param {number}   p             Percentile in [0, 100]
 * @returns {number}
 */
function percentile(sortedValues, p) {
  if (sortedValues.length === 0) return 0;
  if (sortedValues.length === 1) return sortedValues[0];
  const index = (p / 100) * (sortedValues.length - 1);
  const lower = Math.floor(index);
  const upper = Math.ceil(index);
  const frac = index - lower;
  return sortedValues[lower] * (1 - frac) + sortedValues[upper] * frac;
}

/**
 * Classifies a z-score into an anomaly severity level.
 *
 * @param {number} z
 * @param {number} [warnSigma]
 * @param {number} [critSigma]
 * @returns {AnomalySeverity}
 */
export function classifyZScore(z, warnSigma = WARN_SIGMA, critSigma = CRIT_SIGMA) {
  const absZ = Math.abs(z);
  if (absZ >= critSigma) return 'critical';
  if (absZ >= warnSigma) return 'warning';
  return 'normal';
}

/**
 * Annotates a time series of raw metric points with rolling anomaly statistics.
 *
 * Uses an expanding window until `windowSize` points are accumulated, then
 * a sliding window. This ensures early data points still receive reasonable
 * baseline estimates without wait.
 *
 * The algorithm is O(n * windowSize) in the worst case. For dashboards with
 * ≤1000 points per metric (typical K8s scrape intervals) this is negligible.
 * If windowSize is set conservatively (e.g. 60 scrapes = 5 min at 5 s intervals)
 * the hot path for each new live point is O(windowSize), well under 1 ms.
 *
 * @param {MetricPoint[]} points           Time-ordered metric samples
 * @param {number}        [windowSize=60]  Rolling window length
 * @param {number}        [warnSigma]      Warning threshold in σ
 * @param {number}        [critSigma]      Critical threshold in σ
 * @returns {AnomalyPoint[]}
 */
export function annotateAnomalies(
  points,
  windowSize = 60,
  warnSigma = WARN_SIGMA,
  critSigma = CRIT_SIGMA,
) {
  if (points.length === 0) return [];

  const result = [];

  for (let i = 0; i < points.length; i++) {
    const start = Math.max(0, i - windowSize + 1);
    const window = points.slice(start, i + 1).map((p) => p.value);
    const m = mean(window);
    const s = stddev(window, m);

    const upperWarn = m + warnSigma * s;
    const lowerWarn = m - warnSigma * s;
    const upperCrit = m + critSigma * s;
    const lowerCrit = m - critSigma * s;

    const zScore = s > 0 ? (points[i].value - m) / s : 0;

    // Only flag anomalies once the window is large enough to be meaningful.
    const effectiveZ = window.length >= MIN_WINDOW ? zScore : 0;
    const severity = classifyZScore(effectiveZ, warnSigma, critSigma);

    result.push({
      timestamp: points[i].timestamp,
      value: points[i].value,
      mean: m,
      stddev: s,
      upperWarn,
      lowerWarn,
      upperCrit,
      lowerCrit,
      zScore,
      severity,
      isAnomaly: severity !== 'normal',
    });
  }

  return result;
}

/**
 * Computes summary statistics for a historical baseline series.
 *
 * @param {MetricPoint[]} points  Historical metric samples (any order; will be sorted internally)
 * @returns {BaselineStats}
 */
export function computeBaselineStats(points) {
  if (points.length === 0) {
    return { mean: 0, stddev: 0, min: 0, max: 0, p50: 0, p95: 0, p99: 0, count: 0 };
  }

  const values = points.map((p) => p.value);
  const sorted = [...values].sort((a, b) => a - b);
  const m = mean(values);
  const s = stddev(values, m);

  return {
    mean: m,
    stddev: s,
    min: sorted[0],
    max: sorted[sorted.length - 1],
    p50: percentile(sorted, 50),
    p95: percentile(sorted, 95),
    p99: percentile(sorted, 99),
    count: points.length,
  };
}

/**
 * Compares a current observation window against a historical baseline.
 *
 * Useful for the time-range comparison view: shows whether the current period
 * is statistically similar to or diverging from a past reference period.
 *
 * @param {MetricPoint[]} currentPoints    Recent/current metric window
 * @param {MetricPoint[]} baselinePoints   Historical reference window
 * @param {number}        [warnSigma]
 * @param {number}        [critSigma]
 * @returns {BaselineComparison}
 */
export function compareToBaseline(
  currentPoints,
  baselinePoints,
  warnSigma = WARN_SIGMA,
  critSigma = CRIT_SIGMA,
) {
  const currentValues = currentPoints.map((p) => p.value);
  const baselineValues = baselinePoints.map((p) => p.value);

  const currentMean = mean(currentValues);
  const currentStddev = stddev(currentValues);
  const baselineMean = mean(baselineValues);
  const baselineStddev = stddev(baselineValues);

  const deltaMean = currentMean - baselineMean;
  const deltaPct = baselineMean !== 0 ? (deltaMean / baselineMean) * 100 : 0;
  const zScore = baselineStddev > 0 ? (currentMean - baselineMean) / baselineStddev : 0;

  return {
    currentMean,
    currentStddev,
    baselineMean,
    baselineStddev,
    deltaMean,
    deltaPct,
    zScore,
    severity: classifyZScore(zScore, warnSigma, critSigma),
  };
}

/**
 * Filters a list of AnomalyPoint arrays (one per metric series) down to only
 * those that contain at least one anomalous point, and returns them sorted by
 * highest-severity first, then by z-score magnitude.
 *
 * @param {Array<{label: string, points: AnomalyPoint[]}>} series
 * @returns {Array<{label: string, points: AnomalyPoint[], maxSeverity: AnomalySeverity, maxZScore: number}>}
 */
export function rankAnomalousSeries(series) {
  const SEVERITY_RANK = { critical: 2, warning: 1, normal: 0 };

  return series
    .map(({ label, points }) => {
      const anomalousPoints = points.filter((p) => p.isAnomaly);
      if (anomalousPoints.length === 0) return null;

      const maxZScore = Math.max(...anomalousPoints.map((p) => Math.abs(p.zScore)));
      const hasCritical = anomalousPoints.some((p) => p.severity === 'critical');
      const maxSeverity = hasCritical ? 'critical' : 'warning';

      return { label, points, maxSeverity, maxZScore };
    })
    .filter(Boolean)
    .sort((a, b) => {
      const rankDiff = SEVERITY_RANK[b.maxSeverity] - SEVERITY_RANK[a.maxSeverity];
      if (rankDiff !== 0) return rankDiff;
      return b.maxZScore - a.maxZScore;
    });
}
