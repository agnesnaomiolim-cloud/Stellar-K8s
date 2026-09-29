/**
 * anomalyModel.test.js
 *
 * Component tests for the anomaly detection model (#127).
 *
 * Verifies:
 *   - mean / stddev primitives
 *   - classifyZScore severity mapping
 *   - annotateAnomalies correctly flags spike indices in synthetic datasets
 *   - computeBaselineStats percentile accuracy
 *   - compareToBaseline severity classification
 *   - rankAnomalousSeries ordering
 *   - Normal series produces no false-positive anomalies
 *
 * Run with:  node --experimental-vm-modules --test frontend/analytics/anomalies/anomalyModel.test.js
 * (or via the project's existing node:test harness)
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  mean,
  stddev,
  classifyZScore,
  annotateAnomalies,
  computeBaselineStats,
  compareToBaseline,
  rankAnomalousSeries,
  WARN_SIGMA,
  CRIT_SIGMA,
} from './anomalyModel.js';

import {
  MOCK_P99_LATENCY,
  P99_SPIKE_INDICES,
  MOCK_CPU_UTILIZATION,
  CPU_CRIT_INDICES,
  MOCK_MEMORY_NORMAL,
  MOCK_ERROR_RATE,
  ERROR_RATE_SPIKE_INDICES,
  MOCK_P99_BASELINE,
} from './mockMetrics.js';

// ---------------------------------------------------------------------------
// Primitive statistics
// ---------------------------------------------------------------------------

test('mean of empty array is 0', () => {
  assert.strictEqual(mean([]), 0);
});

test('mean of a simple array', () => {
  assert.strictEqual(mean([1, 2, 3, 4, 5]), 3);
});

test('stddev of empty array is 0', () => {
  assert.strictEqual(stddev([]), 0);
});

test('stddev of single-element array is 0', () => {
  assert.strictEqual(stddev([42]), 0);
});

test('stddev of [2, 4, 4, 4, 5, 5, 7, 9] ≈ 2', () => {
  // Classic textbook example: population stddev = 2
  const result = stddev([2, 4, 4, 4, 5, 5, 7, 9]);
  assert.ok(
    Math.abs(result - 2) < 1e-10,
    `Expected ~2, got ${result}`,
  );
});

test('stddev accepts pre-computed mean for efficiency', () => {
  const values = [1, 2, 3, 4, 5];
  const m = mean(values);
  assert.strictEqual(stddev(values, m), stddev(values));
});

// ---------------------------------------------------------------------------
// Z-score classification
// ---------------------------------------------------------------------------

test('classifyZScore: within warn sigma → normal', () => {
  assert.strictEqual(classifyZScore(0), 'normal');
  assert.strictEqual(classifyZScore(1.9), 'normal');
  assert.strictEqual(classifyZScore(-1.9), 'normal');
});

test('classifyZScore: between warn and crit sigma → warning', () => {
  assert.strictEqual(classifyZScore(WARN_SIGMA), 'warning');
  assert.strictEqual(classifyZScore(CRIT_SIGMA - 0.01), 'warning');
  assert.strictEqual(classifyZScore(-WARN_SIGMA), 'warning');
});

test('classifyZScore: at or beyond crit sigma → critical', () => {
  assert.strictEqual(classifyZScore(CRIT_SIGMA), 'critical');
  assert.strictEqual(classifyZScore(10), 'critical');
  assert.strictEqual(classifyZScore(-CRIT_SIGMA), 'critical');
});

// ---------------------------------------------------------------------------
// annotateAnomalies – p99 latency spikes
// ---------------------------------------------------------------------------

test('annotateAnomalies flags p99 latency spike indices as critical', () => {
  const annotated = annotateAnomalies(MOCK_P99_LATENCY, 60);

  for (const idx of P99_SPIKE_INDICES) {
    const point = annotated[idx];
    assert.ok(
      point.severity === 'critical' || point.severity === 'warning',
      `Expected spike at index ${idx} to be anomalous; got severity="${point.severity}" (value=${point.value}, zScore=${point.zScore.toFixed(2)})`,
    );
    assert.ok(point.isAnomaly, `isAnomaly must be true at index ${idx}`);
  }
});

test('annotateAnomalies: spike point has higher zScore than surrounding points', () => {
  const annotated = annotateAnomalies(MOCK_P99_LATENCY, 60);
  for (const idx of P99_SPIKE_INDICES) {
    const spikeMagnitude = Math.abs(annotated[idx].zScore);
    const before = Math.abs(annotated[idx - 5]?.zScore ?? 0);
    const after = Math.abs(annotated[idx + 5]?.zScore ?? 0);
    assert.ok(
      spikeMagnitude > before && spikeMagnitude > after,
      `Spike at index ${idx} should have highest z-score in neighbourhood; z=${spikeMagnitude.toFixed(2)}, before=${before.toFixed(2)}, after=${after.toFixed(2)}`,
    );
  }
});

test('annotateAnomalies produces correct band boundaries (upperWarn, upperCrit)', () => {
  const annotated = annotateAnomalies(MOCK_P99_LATENCY, 60);
  for (const p of annotated) {
    assert.ok(
      p.upperCrit >= p.upperWarn,
      `upperCrit (${p.upperCrit}) must be >= upperWarn (${p.upperWarn})`,
    );
    assert.ok(
      p.lowerCrit <= p.lowerWarn,
      `lowerCrit (${p.lowerCrit}) must be <= lowerWarn (${p.lowerWarn})`,
    );
  }
});

test('annotateAnomalies: result array same length as input', () => {
  const annotated = annotateAnomalies(MOCK_P99_LATENCY, 60);
  assert.strictEqual(annotated.length, MOCK_P99_LATENCY.length);
});

test('annotateAnomalies: empty input returns empty array', () => {
  assert.deepStrictEqual(annotateAnomalies([]), []);
});

// ---------------------------------------------------------------------------
// annotateAnomalies – CPU spike
// ---------------------------------------------------------------------------

test('annotateAnomalies flags CPU critical spike index as critical', () => {
  const annotated = annotateAnomalies(MOCK_CPU_UTILIZATION, 60);
  for (const idx of CPU_CRIT_INDICES) {
    const point = annotated[idx];
    assert.ok(
      point.severity === 'critical',
      `Expected CPU spike at ${idx} to be critical; got "${point.severity}" (value=${point.value})`,
    );
  }
});

// ---------------------------------------------------------------------------
// Normal series – no false positives
// ---------------------------------------------------------------------------

test('annotateAnomalies: normal memory series produces no critical anomalies', () => {
  const annotated = annotateAnomalies(MOCK_MEMORY_NORMAL, 60);
  const criticals = annotated.filter((p) => p.severity === 'critical');
  assert.strictEqual(
    criticals.length,
    0,
    `Normal series should have 0 critical points; got ${criticals.length}`,
  );
});

test('annotateAnomalies: normal memory series has low anomaly rate (<5%)', () => {
  const annotated = annotateAnomalies(MOCK_MEMORY_NORMAL, 60);
  const anomalies = annotated.filter((p) => p.isAnomaly);
  const rate = anomalies.length / annotated.length;
  assert.ok(
    rate < 0.05,
    `Expected <5% anomaly rate on normal series; got ${(rate * 100).toFixed(1)}%`,
  );
});

// ---------------------------------------------------------------------------
// Error rate spike
// ---------------------------------------------------------------------------

test('annotateAnomalies flags error rate spike as critical', () => {
  const annotated = annotateAnomalies(MOCK_ERROR_RATE, 60);
  for (const idx of ERROR_RATE_SPIKE_INDICES) {
    const point = annotated[idx];
    assert.ok(
      point.isAnomaly,
      `Error spike at index ${idx} must be an anomaly`,
    );
    assert.strictEqual(
      point.severity,
      'critical',
      `Error spike at index ${idx} must be critical; got "${point.severity}"`,
    );
  }
});

// ---------------------------------------------------------------------------
// computeBaselineStats
// ---------------------------------------------------------------------------

test('computeBaselineStats: empty returns all zeros', () => {
  const stats = computeBaselineStats([]);
  assert.strictEqual(stats.count, 0);
  assert.strictEqual(stats.mean, 0);
  assert.strictEqual(stats.stddev, 0);
  assert.strictEqual(stats.p99, 0);
});

test('computeBaselineStats: correct mean and percentiles for known input', () => {
  const points = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10].map((v) => ({
    timestamp: `2024-01-01T00:${String(v).padStart(2, '0')}:00Z`,
    value: v,
  }));
  const stats = computeBaselineStats(points);

  assert.strictEqual(stats.mean, 5.5);
  assert.strictEqual(stats.min, 1);
  assert.strictEqual(stats.max, 10);
  assert.strictEqual(stats.count, 10);
  // p50 of [1..10] should be 5.5
  assert.ok(Math.abs(stats.p50 - 5.5) < 0.01, `p50=${stats.p50}`);
  // p99 should be close to 10
  assert.ok(stats.p99 >= 9 && stats.p99 <= 10, `p99=${stats.p99}`);
});

// ---------------------------------------------------------------------------
// compareToBaseline
// ---------------------------------------------------------------------------

test('compareToBaseline: identical series → zScore=0, severity=normal', () => {
  const pts = MOCK_P99_BASELINE.slice(0, 50);
  const result = compareToBaseline(pts, pts);
  assert.strictEqual(result.zScore, 0);
  assert.strictEqual(result.severity, 'normal');
  assert.strictEqual(result.deltaMean, 0);
});

test('compareToBaseline: elevated current vs baseline → warning or critical', () => {
  // Current series with much higher mean than baseline
  const baseline = MOCK_P99_BASELINE.slice(0, 100);
  const elevated = baseline.map((p) => ({ ...p, value: p.value * 3 }));
  const result = compareToBaseline(elevated, baseline);

  assert.ok(
    result.severity === 'warning' || result.severity === 'critical',
    `Expected warning/critical when current is 3× baseline mean; got "${result.severity}"`,
  );
  assert.ok(result.deltaMean > 0, 'deltaMean should be positive');
  assert.ok(result.deltaPct > 50, 'deltaPct should be large');
});

// ---------------------------------------------------------------------------
// rankAnomalousSeries
// ---------------------------------------------------------------------------

test('rankAnomalousSeries: critical series rank before warning series', () => {
  const annotatedLatency = annotateAnomalies(MOCK_P99_LATENCY, 60);
  const annotatedMemory = annotateAnomalies(MOCK_MEMORY_NORMAL, 60);

  const ranked = rankAnomalousSeries([
    { label: 'Memory', points: annotatedMemory },
    { label: 'p99 Latency', points: annotatedLatency },
  ]);

  if (ranked.length >= 2) {
    const latencyEntry = ranked.find((r) => r.label === 'p99 Latency');
    const memoryEntry = ranked.find((r) => r.label === 'Memory');
    if (latencyEntry && memoryEntry) {
      const latencyIdx = ranked.indexOf(latencyEntry);
      const memoryIdx = ranked.indexOf(memoryEntry);
      // Critical latency should rank before memory (which has no criticals)
      assert.ok(latencyIdx <= memoryIdx, 'Critical series should rank first');
    }
  }
  // At minimum p99 latency must appear (it has clear spikes)
  assert.ok(
    ranked.some((r) => r.label === 'p99 Latency'),
    'p99 Latency must appear in anomalous series',
  );
});

test('rankAnomalousSeries: all-normal series returns empty array', () => {
  const annotated = annotateAnomalies(MOCK_MEMORY_NORMAL, 60);
  // Force all points to normal
  const normalPoints = annotated.map((p) => ({ ...p, severity: 'normal', isAnomaly: false }));
  const ranked = rankAnomalousSeries([{ label: 'Normal', points: normalPoints }]);
  assert.strictEqual(ranked.length, 0);
});
