/**
 * AuthNodeCard — renders one node (Address or Contract) on the auth tree
 * canvas, positioned absolutely by the parent per `tree_layout`.
 *
 * Acts as both a drag source (to reparent it under another node) and a drop
 * target (to accept a Contract dropped from the palette, or an existing node
 * being reparented onto it). Inline fields let the author edit the node's
 * data directly on the card — there is no separate "configure then drop"
 * step like the topology configurator; the tree itself is the source of truth.
 */

import React, { useState } from 'react';
import type { AuthArg, AuthArgType, AuthTreeNode } from '../../../utils/auth_validator';
import type { DragPayload } from './types';

export interface AuthNodeCardProps {
  node: AuthTreeNode;
  x: number;
  y: number;
  hasErrors: boolean;
  hasWarnings: boolean;
  onUpdate: (nodeId: string, updates: Partial<AuthTreeNode>) => void;
  onSetArgs: (nodeId: string, args: AuthArg[]) => void;
  onRemove: (nodeId: string) => void;
  onDropPayload: (targetNodeId: string, payload: DragPayload) => void;
}

const ARG_TYPES: AuthArgType[] = ['address', 'string', 'symbol', 'bytes', 'bool', 'u32', 'u64', 'i128'];

const CARD: React.CSSProperties = {
  position: 'absolute',
  width: '220px',
  background: '#111b27',
  border: '1px solid #273340',
  borderRadius: '8px',
  padding: '10px 12px',
  fontFamily: "'Space Grotesk', sans-serif",
  cursor: 'grab',
};

const KIND_BADGE: React.CSSProperties = {
  display: 'inline-block',
  fontSize: '10px',
  fontWeight: 700,
  letterSpacing: '0.05em',
  textTransform: 'uppercase',
  borderRadius: '4px',
  padding: '2px 6px',
  marginBottom: '6px',
};

const INPUT: React.CSSProperties = {
  width: '100%',
  background: '#0b1119',
  border: '1px solid #273340',
  borderRadius: '4px',
  color: '#e8edf2',
  padding: '4px 6px',
  fontSize: '11px',
  fontFamily: "'DM Mono', monospace",
  outline: 'none',
  boxSizing: 'border-box',
  marginBottom: '5px',
};

const ARG_ROW: React.CSSProperties = {
  display: 'flex',
  gap: '4px',
  marginBottom: '4px',
};

const SMALL_BTN: React.CSSProperties = {
  fontSize: '10px',
  padding: '2px 6px',
  borderRadius: '4px',
  border: '1px solid #273340',
  background: '#0b1119',
  color: '#7a8fa8',
  cursor: 'pointer',
};

const REMOVE_BTN: React.CSSProperties = {
  ...SMALL_BTN,
  color: '#f05d5e',
  borderColor: '#f05d5e44',
  position: 'absolute',
  top: '8px',
  right: '8px',
};

const AuthNodeCard: React.FC<AuthNodeCardProps> = ({
  node,
  x,
  y,
  hasErrors,
  hasWarnings,
  onUpdate,
  onSetArgs,
  onRemove,
  onDropPayload,
}) => {
  const [isDragOver, setIsDragOver] = useState(false);
  const accent = node.kind === 'address' ? '#39d98a' : '#4ea8de';

  const borderColor = hasErrors
    ? 'rgba(240,93,94,0.6)'
    : hasWarnings
    ? 'rgba(245,185,66,0.55)'
    : isDragOver
    ? accent
    : '#273340';

  const handleDragStart = (event: React.DragEvent<HTMLDivElement>) => {
    const payload: DragPayload = { type: 'existing-node', kind: node.kind, nodeId: node.id };
    event.dataTransfer.setData('application/x-auth-drag', JSON.stringify(payload));
    event.dataTransfer.effectAllowed = 'move';
  };

  const handleDragOver = (event: React.DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.stopPropagation();
    setIsDragOver(true);
  };

  const handleDrop = (event: React.DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.stopPropagation();
    setIsDragOver(false);
    const raw = event.dataTransfer.getData('application/x-auth-drag');
    if (!raw) return;
    try {
      const payload = JSON.parse(raw) as DragPayload;
      onDropPayload(node.id, payload);
    } catch {
      console.warn('[AuthNodeCard] Failed to parse drag payload', raw);
    }
  };

  const addArg = () => {
    const args = node.args ?? [];
    onSetArgs(node.id, [...args, { name: `arg${args.length + 1}`, type: 'string', value: '' }]);
  };

  const updateArg = (index: number, updates: Partial<AuthArg>) => {
    const args = [...(node.args ?? [])];
    args[index] = { ...args[index], ...updates };
    onSetArgs(node.id, args);
  };

  const removeArg = (index: number) => {
    const args = [...(node.args ?? [])];
    args.splice(index, 1);
    onSetArgs(node.id, args);
  };

  return (
    <div
      style={{ ...CARD, left: x, top: y, borderColor, boxShadow: isDragOver ? `0 0 0 2px ${accent}44` : 'none' }}
      draggable={node.kind === 'contract'}
      onDragStart={handleDragStart}
      onDragOver={handleDragOver}
      onDragLeave={() => setIsDragOver(false)}
      onDrop={handleDrop}
      role="group"
      aria-label={`${node.kind} node ${node.id}`}
    >
      <button
        style={REMOVE_BTN}
        onClick={() => onRemove(node.id)}
        aria-label={`Remove node ${node.id}`}
        title="Remove node"
      >
        ✕
      </button>

      <span
        style={{ ...KIND_BADGE, background: `${accent}22`, color: accent, border: `1px solid ${accent}44` }}
      >
        {node.kind === 'address' ? 'Address (Signer)' : 'Contract Call'}
      </span>

      {node.kind === 'address' ? (
        <input
          style={INPUT}
          type="text"
          value={node.address ?? ''}
          onChange={(e) => onUpdate(node.id, { address: e.target.value })}
          placeholder="GABC... signing account"
          aria-label="Signing account address"
        />
      ) : (
        <>
          <input
            style={INPUT}
            type="text"
            value={node.contractAddress ?? ''}
            onChange={(e) => onUpdate(node.id, { contractAddress: e.target.value })}
            placeholder="CABC... contract address"
            aria-label="Contract address"
          />
          <input
            style={INPUT}
            type="text"
            value={node.functionName ?? ''}
            onChange={(e) => onUpdate(node.id, { functionName: e.target.value })}
            placeholder="function name"
            aria-label="Contract function name"
          />

          {(node.args ?? []).map((arg, i) => (
            <div key={i} style={ARG_ROW}>
              <input
                style={{ ...INPUT, width: '70px', marginBottom: 0 }}
                type="text"
                value={arg.name}
                onChange={(e) => updateArg(i, { name: e.target.value })}
                aria-label={`Argument ${i + 1} name`}
              />
              <select
                style={{ ...INPUT, width: '70px', marginBottom: 0 }}
                value={arg.type}
                onChange={(e) => updateArg(i, { type: e.target.value as AuthArgType })}
                aria-label={`Argument ${i + 1} type`}
              >
                {ARG_TYPES.map((t) => (
                  <option key={t} value={t}>
                    {t}
                  </option>
                ))}
              </select>
              <input
                style={{ ...INPUT, flex: 1, marginBottom: 0 }}
                type="text"
                value={arg.value}
                onChange={(e) => updateArg(i, { value: e.target.value })}
                aria-label={`Argument ${i + 1} value`}
              />
              <button
                style={SMALL_BTN}
                onClick={() => removeArg(i)}
                aria-label={`Remove argument ${i + 1}`}
              >
                ✕
              </button>
            </div>
          ))}

          <button style={{ ...SMALL_BTN, marginTop: '2px' }} onClick={addArg}>
            + arg
          </button>
        </>
      )}
    </div>
  );
};

export default AuthNodeCard;
