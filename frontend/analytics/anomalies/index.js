/**
 * index.js
 *
 * Public API for the anomalies analytics module (#127).
 * Re-exports all stable symbols so consumers can import from one path.
 */

export {
  // Statistical primitives
  mean,
  stddev,
  classifyZScore,

  // Annotation pipeline
  annotateAnomalies,
  computeBaselineStats,
  compareToBaseline,
  rankAnomalousSeries,

  // Constants
  WARN_SIGMA,
  CRIT_SIGMA,
  MIN_WINDOW,
} from './anomalyModel.js';

export {
  // Synthetic datasets
  ALL_MOCK_METRICS,
  MOCK_P99_LATENCY,
  MOCK_P99_BASELINE,
  MOCK_CPU_UTILIZATION,
  MOCK_MEMORY_NORMAL,
  MOCK_ERROR_RATE,

  // Known spike indices (for testing)
  P99_SPIKE_INDICES,
  CPU_WARN_INDICES,
  CPU_CRIT_INDICES,
  ERROR_RATE_SPIKE_INDICES,
} from './mockMetrics.js';

// Default scene component
export { default as AnomalyWorkbench } from './AnomalyWorkbench.jsx';
