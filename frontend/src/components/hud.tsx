import type { GraphSummary } from '../topology/layout';

export interface HudProps {
  summary: GraphSummary | null;
}

/** Top-left network statistics panel. */
export default function Hud({ summary }: HudProps) {
  if (!summary) return null;
  const { nodeCount, edgeCount, directCount, indirectCount, missingCount } = summary;
  const articulation = summary.articulationPoints.length;

  return (
    <div className="topology-hud">
      <div className="topology-hud__stats">
        <div className="topology-hud__row">
          <span>Nodes</span>
          <b>{nodeCount}</b>
        </div>
        <div className="topology-hud__row">
          <span>Edges</span>
          <b>{edgeCount}</b>
        </div>
        <div className="topology-hud__row">
          <span>Direct trust</span>
          <b className="is-good">{directCount}</b>
        </div>
        <div className="topology-hud__row">
          <span>Indirect trust</span>
          <b>{indirectCount}</b>
        </div>
        <div className="topology-hud__row">
          <span>Missing peers</span>
          <b className={missingCount > 0 ? 'is-bad' : 'is-good'}>{missingCount}</b>
        </div>
        <div className="topology-hud__row">
          <span>SPOF candidates</span>
          <b className={articulation > 0 ? 'is-warn' : 'is-good'}>{articulation}</b>
        </div>
        <div className="topology-hud__row">
          <span>Quorum intersection</span>
          <b className={summary.hasQuorumIntersection ? 'is-good' : 'is-bad'}>
            {summary.hasQuorumIntersection ? 'yes' : 'NO'}
          </b>
        </div>
      </div>
    </div>
  );
}
