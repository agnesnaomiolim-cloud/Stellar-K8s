/**
 * anomaly_card.tsx
 *
 * AnomalyCard component for the Anomaly Detection Workbench (#127).
 *
 * Renders a single metric card with:
 *   - A mini Recharts area/line chart showing the time series.
 *   - Shaded std-dev bands (warn and critical) as ReferenceArea overlays.
 *   - Anomalous data points highlighted with distinct fill/stroke.
 *   - A severity badge (normal / warning / critical) surfaced from statistical analysis.
 *   - Optional baseline comparison callout showing delta vs historical period.
 *
 * Designed to be rendered inside the AnomalyWorkbench grid.
 */

import {
  Area,
  AreaChart,
  CartesianGrid,
  Legend,
  ReferenceArea,
  ReferenceLine,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type AnomalySeverity = 'normal' | 'warning' | 'critical';

export interface AnomalyPoint {
  timestamp: string;
  value: number;
  mean: number;
  stddev: number;
  upperWarn: number;
  lowerWarn: number;
  upperCrit: number;
  lowerCrit: number;
  zScore: number;
  severity: AnomalySeverity;
  isAnomaly: boolean;
}

export interface BaselineComparison {
  currentMean: number;
  currentStddev: number;
  baselineMean: number;
  baselineStddev: number;
  deltaMean: number;
  deltaPct: number;
  zScore: number;
  severity: AnomalySeverity;
}

export interface AnomalyCardProps {
  /** Human-readable metric label, e.g. "p99 Latency". */
  label: string;
  /** Unit suffix appended to tooltip values, e.g. "ms" or "%". */
  unit?: string;
  /** Annotated time-series data from `annotateAnomalies()`. */
  points: AnomalyPoint[];
  /** Optional comparison result from `compareToBaseline()`. */
  baselineComparison?: BaselineComparison;
  /** Chart height in px (default 160). */
  height?: number;
  /** Whether to render the expanded detailed view (default false). */
  expanded?: boolean;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SEVERITY_COLORS: Record<AnomalySeverity, string> = {
  normal: '#22c55e',   // green-500
  warning: '#f59e0b',  // amber-500
  critical: '#ef4444', // red-500
};

const SEVERITY_BG: Record<AnomalySeverity, string> = {
  normal: '#14532d',   // dark green
  warning: '#78350f',  // dark amber
  critical: '#7f1d1d', // dark red
};

const BAND_FILL = {
  warn: 'rgba(245,158,11,0.15)',
  crit: 'rgba(239,68,68,0.15)',
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatTimestamp(ts: string): string {
  const d = new Date(ts);
  return d.toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

function formatShortTime(ts: string): string {
  const d = new Date(ts);
  return d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

/**
 * Derives the worst severity across all anomalous points in the series.
 */
function worstSeverity(points: AnomalyPoint[]): AnomalySeverity {
  if (points.some((p) => p.severity === 'critical')) return 'critical';
  if (points.some((p) => p.severity === 'warning')) return 'warning';
  return 'normal';
}

/**
 * Counts anomalous points grouped by severity.
 */
function anomalyCounts(points: AnomalyPoint[]) {
  return {
    critical: points.filter((p) => p.severity === 'critical').length,
    warning: points.filter((p) => p.severity === 'warning').length,
  };
}

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

interface SeverityBadgeProps {
  severity: AnomalySeverity;
  counts: { critical: number; warning: number };
}

function SeverityBadge({ severity, counts }: SeverityBadgeProps) {
  const label =
    severity === 'critical'
      ? `⚠ Critical (${counts.critical} spike${counts.critical !== 1 ? 's' : ''})`
      : severity === 'warning'
      ? `⚡ Warning (${counts.warning} point${counts.warning !== 1 ? 's' : ''})`
      : '✓ Normal';

  return (
    <span
      className="anomaly-card__badge"
      style={{
        backgroundColor: SEVERITY_BG[severity],
        color: SEVERITY_COLORS[severity],
        border: `1px solid ${SEVERITY_COLORS[severity]}`,
      }}
      role="status"
      aria-live="polite"
      aria-label={`${label}`}
    >
      {label}
    </span>
  );
}

interface BaselineCalloutProps {
  comparison: BaselineComparison;
  unit?: string;
}

function BaselineCallout({ comparison, unit = '' }: BaselineCalloutProps) {
  const sign = comparison.deltaMean >= 0 ? '+' : '';
  const pctSign = comparison.deltaPct >= 0 ? '+' : '';
  const color = SEVERITY_COLORS[comparison.severity];

  return (
    <div className="anomaly-card__baseline" aria-label="Baseline comparison">
      <span className="anomaly-card__baseline-label">vs baseline</span>
      <span className="anomaly-card__baseline-delta" style={{ color }}>
        {sign}
        {comparison.deltaMean.toFixed(2)}
        {unit} ({pctSign}
        {comparison.deltaPct.toFixed(1)}%)
      </span>
      <span className="anomaly-card__baseline-zscore" style={{ color }}>
        z={comparison.zScore.toFixed(2)}
      </span>
    </div>
  );
}

// Custom dot renderer: anomalous points rendered with a visible red/amber dot.
function AnomalyDot(props: {
  cx?: number;
  cy?: number;
  payload?: AnomalyPoint;
}) {
  const { cx, cy, payload } = props;
  if (!payload?.isAnomaly || cx === undefined || cy === undefined) return null;
  const color = SEVERITY_COLORS[payload.severity];
  return (
    <circle
      cx={cx}
      cy={cy}
      r={4}
      fill={color}
      stroke="#0f172a"
      strokeWidth={1.5}
      role="img"
      aria-label={`Anomaly: ${payload.value} (${payload.severity})`}
    />
  );
}

// ---------------------------------------------------------------------------
// Main component
// ---------------------------------------------------------------------------

/**
 * AnomalyCard — renders a single metric time series with σ-band overlays
 * and anomaly highlighting.
 */
export function AnomalyCard({
  label,
  unit = '',
  points,
  baselineComparison,
  height = 160,
  expanded = false,
}: AnomalyCardProps) {
  if (points.length === 0) {
    return (
      <div className="anomaly-card anomaly-card--empty" aria-label={`${label} – no data`}>
        <p className="anomaly-card__empty-msg">No data available for {label}.</p>
      </div>
    );
  }

  const severity = worstSeverity(points);
  const counts = anomalyCounts(points);

  // For the σ-band overlays we need stable references (take from last point).
  const lastAnnotated = points[points.length - 1];
  const { upperWarn, lowerWarn, upperCrit, lowerCrit } = lastAnnotated;

  // Build a version of points with explicit anomaly marker key for the dot renderer.
  const chartData = points.map((p) => ({
    ...p,
    // Recharts passes the whole datum to custom dots via `payload`
    __anomaly: p.isAnomaly ? p.value : undefined,
  }));

  // Compute a stable Y-domain with 10% headroom above the critical band.
  const allValues = points.map((p) => p.value);
  const minY = Math.min(...allValues, lowerCrit) * 0.9;
  const maxY = Math.max(...allValues, upperCrit) * 1.1;

  return (
    <article
      className={`anomaly-card anomaly-card--${severity}`}
      aria-label={`${label} metric card – ${severity}`}
    >
      {/* Header */}
      <header className="anomaly-card__header">
        <h3 className="anomaly-card__title">{label}</h3>
        <SeverityBadge severity={severity} counts={counts} />
      </header>

      {/* Baseline comparison callout */}
      {baselineComparison && (
        <BaselineCallout comparison={baselineComparison} unit={unit} />
      )}

      {/* Chart */}
      <div className="anomaly-card__chart" aria-hidden="true">
        <ResponsiveContainer width="100%" height={height}>
          <AreaChart data={chartData} margin={{ top: 4, right: 8, left: 0, bottom: 4 }}>
            <defs>
              <linearGradient id={`grad-${label}`} x1="0" y1="0" x2="0" y2="1">
                <stop offset="5%" stopColor="#38bdf8" stopOpacity={0.3} />
                <stop offset="95%" stopColor="#38bdf8" stopOpacity={0} />
              </linearGradient>
            </defs>

            <CartesianGrid strokeDasharray="3 3" opacity={0.15} />
            <XAxis
              dataKey="timestamp"
              tickFormatter={formatShortTime}
              minTickGap={60}
              tick={{ fontSize: 10, fill: '#94a3b8' }}
            />
            <YAxis
              domain={[Math.max(0, minY), maxY]}
              width={42}
              tick={{ fontSize: 10, fill: '#94a3b8' }}
              tickFormatter={(v: number) => `${v.toFixed(0)}${unit}`}
            />
            <Tooltip
              labelFormatter={(l) => formatTimestamp(l as string)}
              formatter={(value: number, name: string) => [`${Number(value).toFixed(2)} ${unit}`, name]}
              contentStyle={{ backgroundColor: '#1e293b', border: '1px solid #334155', fontSize: 12 }}
            />

            {/* σ-band reference areas – drawn BEFORE the data line so they're behind it */}
            {/* Critical band */}
            <ReferenceArea
              y1={upperWarn}
              y2={upperCrit}
              fill={BAND_FILL.warn}
              ifOverflow="extendDomain"
            />
            <ReferenceArea
              y1={Math.max(0, lowerCrit)}
              y2={lowerWarn}
              fill={BAND_FILL.warn}
              ifOverflow="extendDomain"
            />
            <ReferenceArea
              y1={upperCrit}
              y2={maxY}
              fill={BAND_FILL.crit}
              ifOverflow="extendDomain"
            />
            <ReferenceArea
              y1={Math.max(0, minY)}
              y2={Math.max(0, lowerCrit)}
              fill={BAND_FILL.crit}
              ifOverflow="extendDomain"
            />

            {/* Warning / critical threshold lines */}
            <ReferenceLine
              y={upperWarn}
              stroke={SEVERITY_COLORS.warning}
              strokeDasharray="4 3"
              strokeOpacity={0.6}
              label={{ value: `+2σ`, position: 'insideTopRight', fill: SEVERITY_COLORS.warning, fontSize: 9 }}
            />
            <ReferenceLine
              y={upperCrit}
              stroke={SEVERITY_COLORS.critical}
              strokeDasharray="4 3"
              strokeOpacity={0.6}
              label={{ value: `+3σ`, position: 'insideTopRight', fill: SEVERITY_COLORS.critical, fontSize: 9 }}
            />

            {/* Rolling mean reference line */}
            <ReferenceLine
              y={lastAnnotated.mean}
              stroke="#64748b"
              strokeDasharray="2 4"
              strokeOpacity={0.5}
              label={{ value: 'μ', position: 'insideLeft', fill: '#64748b', fontSize: 9 }}
            />

            {/* Main metric area */}
            <Area
              type="monotone"
              dataKey="value"
              name={label}
              stroke="#38bdf8"
              fill={`url(#grad-${label})`}
              strokeWidth={1.5}
              dot={<AnomalyDot />}
              isAnimationActive={false}
              connectNulls
            />

            {/* Expanded view: also show the rolling mean line */}
            {expanded && (
              <Area
                type="monotone"
                dataKey="mean"
                name="Rolling mean"
                stroke="#64748b"
                fill="none"
                strokeWidth={1}
                dot={false}
                isAnimationActive={false}
                connectNulls
              />
            )}
          </AreaChart>
        </ResponsiveContainer>
      </div>

      {/* Stats summary (shown in expanded mode) */}
      {expanded && (
        <footer className="anomaly-card__stats" aria-label={`${label} statistics`}>
          <dl className="anomaly-card__stats-list">
            <div className="anomaly-card__stats-item">
              <dt>Mean</dt>
              <dd>
                {lastAnnotated.mean.toFixed(2)} {unit}
              </dd>
            </div>
            <div className="anomaly-card__stats-item">
              <dt>σ</dt>
              <dd>
                {lastAnnotated.stddev.toFixed(2)} {unit}
              </dd>
            </div>
            <div className="anomaly-card__stats-item">
              <dt>Last z-score</dt>
              <dd style={{ color: SEVERITY_COLORS[lastAnnotated.severity] }}>
                {lastAnnotated.zScore.toFixed(2)}
              </dd>
            </div>
            <div className="anomaly-card__stats-item">
              <dt>Anomalies</dt>
              <dd>
                {counts.critical > 0 && (
                  <span style={{ color: SEVERITY_COLORS.critical }}>
                    {counts.critical} critical{' '}
                  </span>
                )}
                {counts.warning > 0 && (
                  <span style={{ color: SEVERITY_COLORS.warning }}>
                    {counts.warning} warning
                  </span>
                )}
                {counts.critical === 0 && counts.warning === 0 && (
                  <span style={{ color: SEVERITY_COLORS.normal }}>None</span>
                )}
              </dd>
            </div>
          </dl>
        </footer>
      )}
    </article>
  );
}

export default AnomalyCard;
