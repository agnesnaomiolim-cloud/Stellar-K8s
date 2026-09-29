/**
 * AnomalyWorkbench.jsx
 *
 * Anomaly Detection Workbench dashboard (#127).
 *
 * Main scene/page for the analytics app. Renders:
 *   1. A top-level summary bar showing overall cluster health.
 *   2. A responsive grid of AnomalyCard components — one per metric series.
 *   3. A time-range comparison panel comparing the current window against a
 *      historical baseline (last-hour vs last-day, etc.).
 *
 * Data flow:
 *   - In production: fetched from the Prometheus /api/v1/query_range endpoint.
 *   - During development / testing: populated from mock metric datasets.
 *
 * Performance:
 *   - Statistical annotation (`annotateAnomalies`) runs off the render path in
 *     a useMemo call so React never blocks the main thread during re-renders.
 *   - Chart rendering uses isAnimationActive={false} (set in AnomalyCard) to
 *     prevent stalls when many series are displayed simultaneously.
 */

import { useCallback, useMemo, useState } from 'react';

import { annotateAnomalies, compareToBaseline, rankAnomalousSeries } from './anomalyModel.js';
import {
  ALL_MOCK_METRICS,
  MOCK_P99_BASELINE,
  MOCK_P99_LATENCY,
} from './mockMetrics.js';

// AnomalyCard is in the shared components directory.
// Import path works because analytics/vite.config.js does NOT alias components/
// — the card is imported as a relative path from the analytics scene.
import AnomalyCard from '../../components/anomaly_card.tsx';

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_WINDOW_SIZE = 60; // rolling window in data points

/** Available time-range comparison options */
const COMPARISON_RANGES = [
  { label: 'Last 30 min', currentSamples: 30, baselineSamples: 30 },
  { label: 'Last hour', currentSamples: 60, baselineSamples: 60 },
  { label: 'Last 2 hours', currentSamples: 120, baselineSamples: 120 },
];

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

/**
 * Summary bar showing the cluster-level anomaly count.
 */
function WorkbenchSummary({ totalAnomalies, criticalCount, warningCount }) {
  let status = 'normal';
  if (criticalCount > 0) status = 'critical';
  else if (warningCount > 0) status = 'warning';

  const statusLabel =
    status === 'critical'
      ? `⚠ ${criticalCount} critical metric${criticalCount !== 1 ? 's' : ''}`
      : status === 'warning'
      ? `⚡ ${warningCount} warning metric${warningCount !== 1 ? 's' : ''}`
      : '✓ All metrics normal';

  return (
    <div
      className={`workbench__summary workbench__summary--${status}`}
      role="status"
      aria-live="polite"
      aria-label="Cluster anomaly status"
    >
      <span className="workbench__summary-status">{statusLabel}</span>
      <span className="workbench__summary-detail">
        {totalAnomalies} anomalous point{totalAnomalies !== 1 ? 's' : ''} detected across all
        metrics
      </span>
    </div>
  );
}

/**
 * Time-range comparison panel for a single metric.
 */
function ComparisonPanel({ label, currentPoints, baselinePoints, unit }) {
  const comparison = useMemo(
    () => compareToBaseline(currentPoints, baselinePoints),
    [currentPoints, baselinePoints],
  );

  const sign = comparison.deltaMean >= 0 ? '+' : '';
  const pctSign = comparison.deltaPct >= 0 ? '+' : '';
  const severityColor =
    comparison.severity === 'critical'
      ? '#ef4444'
      : comparison.severity === 'warning'
      ? '#f59e0b'
      : '#22c55e';

  return (
    <div className="comparison-panel" aria-label={`${label} baseline comparison`}>
      <h4 className="comparison-panel__title">{label}</h4>
      <div className="comparison-panel__stats">
        <div className="comparison-panel__stat">
          <span className="comparison-panel__stat-label">Current mean</span>
          <span className="comparison-panel__stat-value">
            {comparison.currentMean.toFixed(2)} {unit}
          </span>
        </div>
        <div className="comparison-panel__stat">
          <span className="comparison-panel__stat-label">Baseline mean</span>
          <span className="comparison-panel__stat-value">
            {comparison.baselineMean.toFixed(2)} {unit}
          </span>
        </div>
        <div className="comparison-panel__stat">
          <span className="comparison-panel__stat-label">Delta</span>
          <span className="comparison-panel__stat-value" style={{ color: severityColor }}>
            {sign}
            {comparison.deltaMean.toFixed(2)} {unit} ({pctSign}
            {comparison.deltaPct.toFixed(1)}%)
          </span>
        </div>
        <div className="comparison-panel__stat">
          <span className="comparison-panel__stat-label">Z-score</span>
          <span className="comparison-panel__stat-value" style={{ color: severityColor }}>
            {comparison.zScore.toFixed(2)}σ
          </span>
        </div>
        <div className="comparison-panel__stat">
          <span className="comparison-panel__stat-label">Verdict</span>
          <span
            className={`comparison-panel__verdict comparison-panel__verdict--${comparison.severity}`}
            style={{ color: severityColor }}
          >
            {comparison.severity.toUpperCase()}
          </span>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Main workbench component
// ---------------------------------------------------------------------------

/**
 * AnomalyWorkbench
 *
 * Full-page anomaly detection dashboard. Props are intentionally minimal so
 * the component can run with mock data out of the box (no API wiring needed
 * for development/testing).
 *
 * @param {object}  props
 * @param {Array}   [props.metrics]       Override mock data with real metric series
 * @param {number}  [props.windowSize]    Rolling window size in samples
 * @param {boolean} [props.useMockData]   Force mock data even when metrics are provided
 */
export default function AnomalyWorkbench({
  metrics = ALL_MOCK_METRICS,
  windowSize = DEFAULT_WINDOW_SIZE,
  useMockData = false,
}) {
  const effectiveMetrics = useMockData ? ALL_MOCK_METRICS : metrics;

  // UI state
  const [expandedCard, setExpandedCard] = useState(null);
  const [selectedRange, setSelectedRange] = useState(COMPARISON_RANGES[1]);
  const [showAllMetrics, setShowAllMetrics] = useState(false);

  // ---------------------------------------------------------------------------
  // Memoised statistical annotation
  // Runs once per `effectiveMetrics` or `windowSize` change, NOT on every render.
  // ---------------------------------------------------------------------------
  const annotatedSeries = useMemo(
    () =>
      effectiveMetrics.map(({ label, unit, points }) => ({
        label,
        unit: unit ?? '',
        points: annotateAnomalies(points, windowSize),
      })),
    [effectiveMetrics, windowSize],
  );

  // Rank series by worst severity so critical metrics always appear first.
  const rankedSeries = useMemo(() => {
    const ranked = rankAnomalousSeries(annotatedSeries);
    const rankedLabels = new Set(ranked.map((r) => r.label));
    // Append non-anomalous series at the end
    const normal = annotatedSeries.filter((s) => !rankedLabels.has(s.label));
    return [...ranked, ...normal];
  }, [annotatedSeries]);

  // Summary counters
  const { totalAnomalies, criticalCount, warningCount } = useMemo(() => {
    let total = 0;
    let crit = 0;
    let warn = 0;
    for (const { points } of annotatedSeries) {
      const anomalies = points.filter((p) => p.isAnomaly);
      total += anomalies.length;
      if (points.some((p) => p.severity === 'critical')) crit++;
      if (points.some((p) => p.severity === 'warning' && p.severity !== 'critical')) warn++;
    }
    return { totalAnomalies: total, criticalCount: crit, warningCount: warn };
  }, [annotatedSeries]);

  // Baseline comparison data (p99 latency vs last-week baseline)
  const p99AnnotatedPoints = useMemo(
    () => annotateAnomalies(MOCK_P99_LATENCY, windowSize),
    [windowSize],
  );
  const baselinePoints = useMemo(() => annotateAnomalies(MOCK_P99_BASELINE, windowSize), [windowSize]);

  const currentWindow = useMemo(
    () => p99AnnotatedPoints.slice(-selectedRange.currentSamples),
    [p99AnnotatedPoints, selectedRange.currentSamples],
  );
  const baselineWindow = useMemo(
    () => baselinePoints.slice(-selectedRange.baselineSamples),
    [baselinePoints, selectedRange.baselineSamples],
  );

  const handleCardToggle = useCallback(
    (label) => setExpandedCard((prev) => (prev === label ? null : label)),
    [],
  );

  // Number of cards visible when not showing all
  const INITIAL_VISIBLE = 6;
  const visibleSeries = showAllMetrics ? rankedSeries : rankedSeries.slice(0, INITIAL_VISIBLE);

  return (
    <div className="workbench" aria-label="Anomaly Detection Workbench">
      {/* Page header */}
      <header className="workbench__header">
        <h1 className="workbench__title">Anomaly Detection Workbench</h1>
        <p className="workbench__subtitle">
          Statistical baseline deviation detection across cluster metrics.{' '}
          <span className="workbench__window-info">
            Rolling window: {windowSize} samples
          </span>
        </p>
      </header>

      {/* Summary status bar */}
      <WorkbenchSummary
        totalAnomalies={totalAnomalies}
        criticalCount={criticalCount}
        warningCount={warningCount}
      />

      {/* Metric card grid */}
      <section className="workbench__grid" aria-label="Metric anomaly cards">
        {visibleSeries.map(({ label, unit, points }) => (
          <div
            key={label}
            className={`workbench__grid-item${expandedCard === label ? ' workbench__grid-item--expanded' : ''}`}
          >
            <AnomalyCard
              label={label}
              unit={unit}
              points={points}
              expanded={expandedCard === label}
              height={expandedCard === label ? 280 : 160}
            />
            <button
              className="workbench__expand-btn"
              onClick={() => handleCardToggle(label)}
              aria-expanded={expandedCard === label}
              aria-label={`${expandedCard === label ? 'Collapse' : 'Expand'} ${label} card`}
            >
              {expandedCard === label ? '▲ Collapse' : '▼ Details'}
            </button>
          </div>
        ))}
      </section>

      {/* Show more / less toggle */}
      {rankedSeries.length > INITIAL_VISIBLE && (
        <div className="workbench__show-more">
          <button
            className="workbench__show-more-btn"
            onClick={() => setShowAllMetrics((v) => !v)}
            aria-label={showAllMetrics ? 'Show fewer metrics' : `Show all ${rankedSeries.length} metrics`}
          >
            {showAllMetrics
              ? 'Show fewer metrics'
              : `Show all ${rankedSeries.length} metrics (${rankedSeries.length - INITIAL_VISIBLE} more)`}
          </button>
        </div>
      )}

      {/* Time-range comparison panel */}
      <section className="workbench__comparison" aria-label="Time-range baseline comparison">
        <header className="workbench__comparison-header">
          <h2 className="workbench__comparison-title">Historical Baseline Comparison</h2>
          <div
            className="workbench__comparison-range-selector"
            role="group"
            aria-label="Time range selector"
          >
            {COMPARISON_RANGES.map((range) => (
              <button
                key={range.label}
                className={`workbench__range-btn${selectedRange === range ? ' workbench__range-btn--active' : ''}`}
                onClick={() => setSelectedRange(range)}
                aria-pressed={selectedRange === range}
              >
                {range.label}
              </button>
            ))}
          </div>
        </header>

        <div className="workbench__comparison-panels">
          <ComparisonPanel
            label="p99 Latency"
            unit="ms"
            currentPoints={currentWindow}
            baselinePoints={baselineWindow}
          />
          {/* Additional comparison panels can be added here for other metrics */}
        </div>
      </section>
    </div>
  );
}
