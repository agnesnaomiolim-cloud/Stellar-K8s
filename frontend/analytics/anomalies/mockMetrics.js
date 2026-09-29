/**
 * mockMetrics.js
 *
 * Synthetic metric datasets for the Anomaly Detection Workbench (#127).
 *
 * Each generator returns a `MetricPoint[]` array with deliberate artificial
 * spikes at known positions so the test suite can assert exact detection
 * results without network or time dependencies.
 *
 * Spike strategy:
 *   - Background noise: Gaussian-approximate random walk around a stable mean.
 *   - Spike injection: hard-set values at nominated indices to ±N σ beyond
 *     the background, well outside the warning / critical thresholds.
 *
 * @module mockMetrics
 */

/**
 * Simple seeded pseudo-random number generator (mulberry32).
 * Reproducible across environments — no `Math.random()`.
 *
 * @param {number} seed
 * @returns {() => number}  Returns values in [0, 1)
 */
function seededRng(seed) {
  let s = seed >>> 0;
  return function () {
    s += 0x6d2b79f5;
    let z = s;
    z = Math.imul(z ^ (z >>> 15), z | 1);
    z ^= z + Math.imul(z ^ (z >>> 7), z | 61);
    return ((z ^ (z >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * Approximates a standard-normal sample via Box-Muller transform.
 * Uses the provided uniform-random function.
 *
 * @param {() => number} rand
 * @returns {number}
 */
function gaussianSample(rand) {
  // Box-Muller transform
  const u1 = Math.max(1e-10, rand());
  const u2 = rand();
  return Math.sqrt(-2 * Math.log(u1)) * Math.cos(2 * Math.PI * u2);
}

/**
 * Builds an ISO-8601 timestamp string at `baseMs + i * intervalMs`.
 *
 * @param {number} baseMs
 * @param {number} i
 * @param {number} intervalMs
 * @returns {string}
 */
function ts(baseMs, i, intervalMs) {
  return new Date(baseMs + i * intervalMs).toISOString();
}

/**
 * @typedef {Object} SpikeSpec
 * @property {number} index     Index in the generated series at which the spike occurs
 * @property {number} magnitude Value to inject (raw metric value, not relative)
 */

/**
 * Generates a synthetic time series with optional artificial spikes.
 *
 * @param {object}     opts
 * @param {number}     opts.count          Number of data points
 * @param {number}     opts.mean           Background mean of the metric
 * @param {number}     opts.stddev         Background noise standard deviation
 * @param {SpikeSpec[]} [opts.spikes]      Explicit spike overrides
 * @param {number}     [opts.seed]         RNG seed for reproducibility
 * @param {number}     [opts.baseMs]       Base timestamp in ms (default: epoch-anchored for stability)
 * @param {number}     [opts.intervalMs]   Sampling interval in ms (default: 15 000 = 15 s)
 * @returns {{ timestamp: string, value: number }[]}
 */
function generateSeries({
  count,
  mean: mu,
  stddev: sigma,
  spikes = [],
  seed = 42,
  baseMs = 1_700_000_000_000, // Fixed epoch for reproducibility
  intervalMs = 15_000,
}) {
  const rand = seededRng(seed);
  const spikeMap = new Map(spikes.map((s) => [s.index, s.magnitude]));

  return Array.from({ length: count }, (_, i) => {
    const noise = gaussianSample(rand) * sigma;
    const raw = mu + noise;
    const value = spikeMap.has(i) ? spikeMap.get(i) : Math.max(0, raw);
    return { timestamp: ts(baseMs, i, intervalMs), value };
  });
}

// ---------------------------------------------------------------------------
// Named synthetic datasets (publicly exported)
// ---------------------------------------------------------------------------

/**
 * p99 API latency (ms) with two artificial latency spikes.
 * Expected: the detector flags indices 70 and 130 as critical.
 *
 * Baseline: ~120 ms mean, ~15 ms noise.
 * Spike 1 (index 70): 450 ms  (~22 σ)
 * Spike 2 (index 130): 380 ms (~17 σ)
 */
export const MOCK_P99_LATENCY = generateSeries({
  count: 200,
  mean: 120,
  stddev: 15,
  seed: 1,
  spikes: [
    { index: 70, magnitude: 450 },
    { index: 130, magnitude: 380 },
  ],
});

/**
 * Known spike indices in MOCK_P99_LATENCY for assertion in tests.
 */
export const P99_SPIKE_INDICES = [70, 130];

/**
 * CPU utilization (%) with a gradual ramp-up and one sharp warning spike.
 * Expected: indices around 85–90 are flagged as warning; index 150 as critical.
 *
 * Baseline: ~35% mean, ~5% noise.
 * Warning ramp: indices 80–90 set to ~55% (between 2–3 σ)
 * Critical spike (index 150): 90%
 */
export const MOCK_CPU_UTILIZATION = (() => {
  const base = generateSeries({
    count: 200,
    mean: 35,
    stddev: 5,
    seed: 2,
  });
  // Overlay the ramp
  for (let i = 80; i <= 90; i++) {
    base[i] = { ...base[i], value: 55 + (i - 80) * 0.5 };
  }
  // Hard spike
  base[150] = { ...base[150], value: 90 };
  return base;
})();

/**
 * Known warning-range indices in MOCK_CPU_UTILIZATION.
 */
export const CPU_WARN_INDICES = [80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90];

/**
 * Known critical spike index in MOCK_CPU_UTILIZATION.
 */
export const CPU_CRIT_INDICES = [150];

/**
 * Memory utilization (%) – steady state with no anomalies.
 * Used to verify that normal series are NOT flagged.
 *
 * Baseline: ~62% mean, ~3% noise.
 */
export const MOCK_MEMORY_NORMAL = generateSeries({
  count: 200,
  mean: 62,
  stddev: 3,
  seed: 3,
});

/**
 * Error rate (errors/s) – near-zero baseline with a single critical burst.
 *
 * Baseline: ~0.5 err/s, ~0.3 noise.
 * Spike (index 100): 8.5 err/s (~27 σ)
 */
export const MOCK_ERROR_RATE = generateSeries({
  count: 200,
  mean: 0.5,
  stddev: 0.3,
  seed: 4,
  spikes: [{ index: 100, magnitude: 8.5 }],
});

/**
 * Known spike index in MOCK_ERROR_RATE.
 */
export const ERROR_RATE_SPIKE_INDICES = [100];

/**
 * Historical baseline for p99 latency (simulates "last week" data).
 * Used by the time-range comparison view.
 *
 * Same distribution as MOCK_P99_LATENCY background but different seed
 * (no spikes — "last week was normal").
 */
export const MOCK_P99_BASELINE = generateSeries({
  count: 200,
  mean: 115,
  stddev: 12,
  seed: 10,
  baseMs: 1_700_000_000_000 - 7 * 24 * 60 * 60 * 1_000, // one week earlier
});

/**
 * All synthetic datasets bundled together for the workbench dashboard.
 *
 * @type {Array<{label: string, unit: string, points: {timestamp: string, value: number}[]}>}
 */
export const ALL_MOCK_METRICS = [
  { label: 'p99 Latency', unit: 'ms', points: MOCK_P99_LATENCY },
  { label: 'CPU Utilization', unit: '%', points: MOCK_CPU_UTILIZATION },
  { label: 'Memory Utilization', unit: '%', points: MOCK_MEMORY_NORMAL },
  { label: 'Error Rate', unit: 'err/s', points: MOCK_ERROR_RATE },
];
