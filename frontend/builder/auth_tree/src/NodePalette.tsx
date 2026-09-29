/**
 * NodePalette — sidebar with two draggable tiles: "Address (Signer)" and
 * "Contract Call". Dragging Address onto empty canvas creates a new tree
 * root; dragging Contract Call onto an existing node (address or contract)
 * nests it as a root invocation or sub-invocation respectively.
 */

import React from 'react';
import type { AuthNodeKind } from '../../../utils/auth_validator';
import type { DragPayload } from './types';

const SIDEBAR: React.CSSProperties = {
  display: 'flex',
  flexDirection: 'column',
  fontFamily: "'Space Grotesk', sans-serif",
};

const HEADER: React.CSSProperties = {
  fontSize: '11px',
  fontWeight: 700,
  letterSpacing: '0.08em',
  textTransform: 'uppercase',
  color: '#556677',
  padding: '0 0 10px 0',
  borderBottom: '1px solid #273340',
  marginBottom: '12px',
};

const TILE: React.CSSProperties = {
  borderRadius: '6px',
  padding: '11px 12px',
  cursor: 'grab',
  userSelect: 'none',
  border: '1px solid #273340',
  background: '#111b27',
  marginBottom: '8px',
  borderLeftWidth: '3px',
};

const TILE_LABEL: React.CSSProperties = {
  fontWeight: 700,
  fontSize: '13px',
  color: '#e8edf2',
  margin: 0,
};

const TILE_DESC: React.CSSProperties = {
  fontSize: '11px',
  color: '#7a8fa8',
  margin: '3px 0 0 0',
  lineHeight: 1.4,
};

const TILES: Array<{ kind: AuthNodeKind; label: string; desc: string; accent: string }> = [
  {
    kind: 'address',
    label: 'Address (Signer)',
    desc: 'Drop on empty canvas to start a new authorization tree rooted at this account.',
    accent: '#39d98a',
  },
  {
    kind: 'contract',
    label: 'Contract Call',
    desc: 'Drop on an Address to set its root invocation, or on a Contract node to nest a sub-invocation.',
    accent: '#4ea8de',
  },
];

const NodePalette: React.FC = () => {
  const handleDragStart = (kind: AuthNodeKind, event: React.DragEvent<HTMLDivElement>) => {
    const payload: DragPayload = { type: 'palette', kind, nodeId: null };
    event.dataTransfer.setData('application/x-auth-drag', JSON.stringify(payload));
    event.dataTransfer.effectAllowed = 'copy';
  };

  return (
    <aside style={SIDEBAR} role="complementary" aria-label="Auth tree node palette">
      <p style={HEADER}>Node Types</p>
      {TILES.map((t) => (
        <div
          key={t.kind}
          style={{ ...TILE, borderLeftColor: t.accent }}
          draggable
          onDragStart={(e) => handleDragStart(t.kind, e)}
          role="button"
          tabIndex={0}
          aria-label={`${t.label} tile. Drag onto the canvas or an existing node.`}
        >
          <p style={TILE_LABEL}>{t.label}</p>
          <p style={TILE_DESC}>{t.desc}</p>
        </div>
      ))}
    </aside>
  );
};

export default NodePalette;
