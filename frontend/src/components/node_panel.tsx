import type { GraphNode } from '../topology/types';

export interface NodePanelProps {
  node: GraphNode | null;
  onClose: () => void;
}

/** Right-hand inspector for the selected node's trust metadata. */
export default function NodePanel({ node, onClose }: NodePanelProps) {
  if (!node) return null;

  const total = node.trusters + node.trusting;
  const isHub = total >= 12;
  const isCritical = node.isArticulationPoint;

  return (
    <aside className="node-panel" aria-label="Node metadata">
      <div className="node-panel__header">
        <div>
          <h2 className="node-panel__title">{node.label}</h2>
          <p className="node-panel__label" style={{ margin: 0 }}>
            {node.domain ?? 'no home domain'}
          </p>
        </div>
        <button className="node-panel__close" onClick={onClose} aria-label="Close">
          ✕
        </button>
      </div>

      <div className="node-panel__badges">
        {isCritical && <span className="node-badge node-badge--critical">⚠ single-point-of-failure</span>}
        {isHub && <span className="node-badge node-badge--hub">● hub</span>}
        {!isCritical && !isHub && <span className="node-badge node-badge--ok">✓ standard</span>}
      </div>

      <div className="node-panel__section">
        <div className="node-panel__label">Trust</div>
        <div className="node-panel__kv">
          <span>Trusting (out)</span>
          <b>{node.trusting}</b>
        </div>
        <div className="node-panel__kv">
          <span>Trusters (in)</span>
          <b>{node.trusters}</b>
        </div>
        <div className="node-panel__kv">
          <span>Total degree</span>
          <b>{total}</b>
        </div>
      </div>

      <div className="node-panel__section">
        <div className="node-panel__label">Observability</div>
        <div className="node-panel__kv">
          <span>Published qset</span>
          <b>{node.hasQuorumSet ? 'yes' : 'no'}</b>
        </div>
        <div className="node-panel__kv">
          <span>Articulation point</span>
          <b>{node.isArticulationPoint ? 'yes' : 'no'}</b>
        </div>
      </div>

      <div className="node-panel__section">
        <div className="node-panel__label">Public key</div>
        <div className="node-panel__list">
          <li>{node.id}</li>
        </div>
      </div>
    </aside>
  );
}
