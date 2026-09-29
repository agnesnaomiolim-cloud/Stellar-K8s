/**
 * AuthTreeBuilder — main orchestrator for the Soroban Auth Hierarchy Builder.
 *
 * Wires together:
 *   - `useAuthTree()` for tree state + dispatch
 *   - `NodePalette` — draggable Address / Contract Call tiles
 *   - A pannable, zoomable canvas rendering the tree via `tree_layout` and
 *     SVG connector lines
 *   - `validateAuthTree` — real-time structural validation
 *   - `buildAuthEntries` — JSON payload generation + export modal
 *
 * Drag-and-drop protocol:
 *   - Dragging the Address tile onto empty canvas creates a new root.
 *   - Dragging the Contract Call tile onto an Address or Contract node adds
 *     it as a child (root invocation or sub-invocation).
 *   - Dragging an existing Contract node onto another node reparents it.
 *   Tree depth is unbounded; the canvas pans/zooms to accommodate large trees.
 */

import React, { useCallback, useMemo, useState, useRef } from 'react';
import { useAuthTree, generateNodeId } from './auth_tree_store';
import { layoutForest } from './tree_layout';
import NodePalette from './NodePalette';
import AuthNodeCard from './AuthNodeCard';
import {
  validateAuthTree,
  buildAuthEntries,
  type AuthArg,
  type AuthTreeNode,
} from '../../../utils/auth_validator';
import type { CanvasTransform, DragPayload } from './types';
import { DEFAULT_TRANSFORM, MIN_SCALE, MAX_SCALE } from './types';

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

const ROOT: React.CSSProperties = {
  display: 'flex',
  flexDirection: 'column',
  height: '100%',
  minHeight: '100vh',
  background: '#0b1119',
  color: '#e8edf2',
  fontFamily: "'Space Grotesk', sans-serif",
};

const TOPBAR: React.CSSProperties = {
  display: 'flex',
  alignItems: 'center',
  justifyContent: 'space-between',
  padding: '12px 20px',
  background: '#0d1623',
  borderBottom: '1px solid #1e2d3d',
  gap: '12px',
  flexWrap: 'wrap',
};

const TITLE: React.CSSProperties = { margin: 0, fontSize: '18px', fontWeight: 700 };
const SUBTITLE: React.CSSProperties = { margin: 0, fontSize: '12px', color: '#7a8fa8' };

const BTN_BASE: React.CSSProperties = {
  padding: '7px 14px',
  borderRadius: '5px',
  border: '1px solid #273340',
  background: '#111b27',
  color: '#c8d8e8',
  cursor: 'pointer',
  fontSize: '12px',
  fontWeight: 600,
  fontFamily: "'Space Grotesk', sans-serif",
};

const BTN_PRIMARY: React.CSSProperties = {
  ...BTN_BASE,
  background: '#39d98a',
  color: '#0b1119',
  border: '1px solid #39d98a',
};

const BTN_DISABLED: React.CSSProperties = {
  ...BTN_BASE,
  color: '#334455',
  borderColor: '#1e2d3d',
  cursor: 'not-allowed',
};

const BODY: React.CSSProperties = { display: 'flex', flex: '1 1 auto', overflow: 'hidden' };

const SIDEBAR: React.CSSProperties = {
  width: '260px',
  flexShrink: 0,
  borderRight: '1px solid #1e2d3d',
  background: '#0d1623',
  padding: '16px 14px',
  overflowY: 'auto',
};

const MAIN: React.CSSProperties = { flex: '1 1 auto', display: 'flex', flexDirection: 'column', overflow: 'hidden' };

const CANVAS_VIEWPORT: React.CSSProperties = {
  flex: '1 1 auto',
  position: 'relative',
  overflow: 'hidden',
  background:
    'radial-gradient(circle, #1a2637 1px, transparent 1px) 0 0 / 24px 24px, #0e1820',
};

const CANVAS_WORLD_BASE: React.CSSProperties = {
  position: 'absolute',
  top: 0,
  left: 0,
};

const ZOOM_CONTROLS: React.CSSProperties = {
  position: 'absolute',
  bottom: '16px',
  right: '16px',
  display: 'flex',
  gap: '6px',
  zIndex: 5,
};

const VALIDATION_PANEL: React.CSSProperties = {
  background: '#0d1623',
  borderTop: '1px solid #1e2d3d',
  padding: '10px 16px',
  maxHeight: '160px',
  overflowY: 'auto',
};

const MODAL_OVERLAY: React.CSSProperties = {
  position: 'fixed',
  inset: 0,
  background: 'rgba(0,0,0,0.72)',
  zIndex: 1000,
  display: 'flex',
  alignItems: 'center',
  justifyContent: 'center',
  padding: '24px',
};

const MODAL: React.CSSProperties = {
  background: '#0d1623',
  border: '1px solid #273340',
  borderRadius: '10px',
  width: '100%',
  maxWidth: '760px',
  maxHeight: '85vh',
  display: 'flex',
  flexDirection: 'column',
  overflow: 'hidden',
};

const JSON_PRE: React.CSSProperties = {
  margin: 0,
  padding: '16px 18px',
  fontSize: '12px',
  lineHeight: 1.6,
  fontFamily: "'DM Mono', monospace",
  color: '#b8cfe8',
  background: '#080f18',
  whiteSpace: 'pre',
  overflow: 'auto',
  minHeight: '200px',
};

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

const AuthTreeBuilder: React.FC = () => {
  const [state, dispatch] = useAuthTree();
  const [transform, setTransform] = useState<CanvasTransform>(DEFAULT_TRANSFORM);
  const [showExport, setShowExport] = useState(false);
  const [copySuccess, setCopySuccess] = useState(false);
  const panRef = useRef<{ startX: number; startY: number; originX: number; originY: number } | null>(null);

  const validation = useMemo(() => validateAuthTree(state.roots), [state.roots]);
  const { positions, edges } = useMemo(() => layoutForest(state.roots), [state.roots]);

  const nodeErrorMap = useMemo(() => buildNodeFlagMap(validation.errors), [validation.errors]);
  const nodeWarningMap = useMemo(() => buildNodeFlagMap(validation.warnings), [validation.warnings]);

  // -------------------------------------------------------------------------
  // Pan handlers
  // -------------------------------------------------------------------------

  const handleCanvasMouseDown = useCallback(
    (event: React.MouseEvent<HTMLDivElement>) => {
      if (event.button !== 0) return;
      panRef.current = {
        startX: event.clientX,
        startY: event.clientY,
        originX: transform.x,
        originY: transform.y,
      };
    },
    [transform.x, transform.y],
  );

  const handleCanvasMouseMove = useCallback((event: React.MouseEvent<HTMLDivElement>) => {
    if (!panRef.current) return;
    const dx = event.clientX - panRef.current.startX;
    const dy = event.clientY - panRef.current.startY;
    setTransform((prev) => ({ ...prev, x: panRef.current!.originX + dx, y: panRef.current!.originY + dy }));
  }, []);

  const handleCanvasMouseUp = useCallback(() => {
    panRef.current = null;
  }, []);

  const handleWheel = useCallback((event: React.WheelEvent<HTMLDivElement>) => {
    event.preventDefault();
    setTransform((prev) => {
      const delta = event.deltaY > 0 ? -0.1 : 0.1;
      const scale = Math.min(MAX_SCALE, Math.max(MIN_SCALE, prev.scale + delta));
      return { ...prev, scale };
    });
  }, []);

  const zoomBy = useCallback((delta: number) => {
    setTransform((prev) => ({
      ...prev,
      scale: Math.min(MAX_SCALE, Math.max(MIN_SCALE, prev.scale + delta)),
    }));
  }, []);

  const resetView = useCallback(() => setTransform(DEFAULT_TRANSFORM), []);

  // -------------------------------------------------------------------------
  // Drop handlers
  // -------------------------------------------------------------------------

  const parseDrag = (event: React.DragEvent): DragPayload | null => {
    const raw = event.dataTransfer.getData('application/x-auth-drag');
    if (!raw) return null;
    try {
      return JSON.parse(raw) as DragPayload;
    } catch {
      return null;
    }
  };

  const handleCanvasDrop = useCallback(
    (event: React.DragEvent<HTMLDivElement>) => {
      event.preventDefault();
      const payload = parseDrag(event);
      if (!payload) return;
      if (payload.type === 'palette' && payload.kind === 'address') {
        dispatch({ type: 'ADD_ROOT_ADDRESS', payload: { id: generateNodeId('addr') } });
      }
      // Dropping a Contract tile (or reparenting) onto empty canvas is a no-op —
      // contract nodes must always be nested under an Address or another Contract.
    },
    [dispatch],
  );

  const handleNodeDrop = useCallback(
    (targetNodeId: string, payload: DragPayload) => {
      if (payload.type === 'palette' && payload.kind === 'contract') {
        dispatch({
          type: 'ADD_CHILD',
          payload: { parentId: targetNodeId, id: generateNodeId('call'), kind: 'contract' },
        });
      } else if (payload.type === 'existing-node' && payload.nodeId) {
        dispatch({ type: 'REPARENT_NODE', payload: { nodeId: payload.nodeId, newParentId: targetNodeId } });
      }
    },
    [dispatch],
  );

  const handleUpdate = useCallback(
    (nodeId: string, updates: Partial<AuthTreeNode>) => {
      dispatch({ type: 'UPDATE_NODE', payload: { nodeId, updates } });
    },
    [dispatch],
  );

  const handleSetArgs = useCallback(
    (nodeId: string, args: AuthArg[]) => {
      dispatch({ type: 'SET_ARGS', payload: { nodeId, args } });
    },
    [dispatch],
  );

  const handleRemove = useCallback(
    (nodeId: string) => {
      dispatch({ type: 'REMOVE_NODE', payload: { nodeId } });
    },
    [dispatch],
  );

  // -------------------------------------------------------------------------
  // Export
  // -------------------------------------------------------------------------

  const exportedJson = useMemo(() => {
    if (!validation.valid) return '';
    return JSON.stringify(buildAuthEntries(state.roots), null, 2);
  }, [state.roots, validation.valid]);

  const handleCopy = useCallback(() => {
    navigator.clipboard
      .writeText(exportedJson)
      .then(() => {
        setCopySuccess(true);
        setTimeout(() => setCopySuccess(false), 2000);
      })
      .catch(() => {});
  }, [exportedJson]);

  const nodesById = useMemo(() => indexNodes(state.roots), [state.roots]);
  const canExport = validation.valid && state.roots.length > 0;

  return (
    <div style={ROOT} role="application" aria-label="Soroban Auth Hierarchy Builder">
      <header style={TOPBAR}>
        <div>
          <h1 style={TITLE}>Auth Hierarchy Builder</h1>
          <p style={SUBTITLE}>
            Drag an Address onto the canvas, then drag Contract Calls onto it to model
            require_auth invocation chains.
          </p>
        </div>
        <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
          <span style={{ fontSize: '11px', color: '#7a8fa8' }}>
            {validation.errors.length > 0
              ? `${validation.errors.length} error(s)`
              : validation.warnings.length > 0
              ? `${validation.warnings.length} warning(s)`
              : 'Valid'}
          </span>
          <button style={{ ...BTN_BASE }} onClick={() => dispatch({ type: 'RESET' })}>
            ↺ Reset
          </button>
          <button
            style={canExport ? BTN_PRIMARY : BTN_DISABLED}
            disabled={!canExport}
            onClick={() => canExport && setShowExport(true)}
          >
            ⬇ Export JSON
          </button>
        </div>
      </header>

      <div style={BODY}>
        <nav style={SIDEBAR}>
          <NodePalette />
        </nav>

        <div style={MAIN}>
          <div
            style={CANVAS_VIEWPORT}
            onMouseDown={handleCanvasMouseDown}
            onMouseMove={handleCanvasMouseMove}
            onMouseUp={handleCanvasMouseUp}
            onMouseLeave={handleCanvasMouseUp}
            onWheel={handleWheel}
            onDragOver={(e) => e.preventDefault()}
            onDrop={handleCanvasDrop}
            role="region"
            aria-label="Auth tree canvas — pan by dragging, zoom with the scroll wheel"
          >
            <div
              style={{
                ...CANVAS_WORLD_BASE,
                transform: `translate(${transform.x}px, ${transform.y}px) scale(${transform.scale})`,
                transformOrigin: '0 0',
              }}
            >
              <svg
                style={{ position: 'absolute', overflow: 'visible', pointerEvents: 'none' }}
                aria-hidden="true"
              >
                {edges.map((edge) => {
                  const from = positions.find((p) => p.id === edge.parentId);
                  const to = positions.find((p) => p.id === edge.childId);
                  if (!from || !to) return null;
                  const x1 = from.x + 220;
                  const y1 = from.y + 30;
                  const x2 = to.x;
                  const y2 = to.y + 30;
                  const midX = (x1 + x2) / 2;
                  return (
                    <path
                      key={`${edge.parentId}-${edge.childId}`}
                      d={`M ${x1} ${y1} C ${midX} ${y1}, ${midX} ${y2}, ${x2} ${y2}`}
                      stroke="#273340"
                      strokeWidth={2}
                      fill="none"
                    />
                  );
                })}
              </svg>

              {positions.map((pos) => {
                const node = nodesById.get(pos.id);
                if (!node) return null;
                return (
                  <AuthNodeCard
                    key={node.id}
                    node={node}
                    x={pos.x}
                    y={pos.y}
                    hasErrors={(nodeErrorMap.get(node.id)?.length ?? 0) > 0}
                    hasWarnings={(nodeWarningMap.get(node.id)?.length ?? 0) > 0}
                    onUpdate={handleUpdate}
                    onSetArgs={handleSetArgs}
                    onRemove={handleRemove}
                    onDropPayload={handleNodeDrop}
                  />
                );
              })}

              {state.roots.length === 0 && (
                <p style={{ color: '#445566', fontSize: '13px', padding: '24px' }}>
                  Drag an "Address (Signer)" tile here to start a tree.
                </p>
              )}
            </div>

            <div style={ZOOM_CONTROLS}>
              <button style={BTN_BASE} onClick={() => zoomBy(-0.1)} aria-label="Zoom out">
                −
              </button>
              <button style={BTN_BASE} onClick={resetView} aria-label="Reset view">
                {Math.round(transform.scale * 100)}%
              </button>
              <button style={BTN_BASE} onClick={() => zoomBy(0.1)} aria-label="Zoom in">
                +
              </button>
            </div>
          </div>

          <aside style={VALIDATION_PANEL} aria-label="Validation results">
            {validation.errors.length === 0 && validation.warnings.length === 0 && (
              <p style={{ margin: 0, fontSize: '12px', color: '#39d98a' }}>
                ✓ No issues found. Tree is ready to export.
              </p>
            )}
            {validation.errors.map((err) => (
              <p key={err.code + err.nodeIds.join(',')} style={{ margin: '2px 0', fontSize: '12px', color: '#f05d5e' }}>
                ✗ {err.message}
              </p>
            ))}
            {validation.warnings.map((warn) => (
              <p key={warn.code + warn.nodeIds.join(',')} style={{ margin: '2px 0', fontSize: '12px', color: '#f5b942' }}>
                ⚠ {warn.message}
              </p>
            ))}
          </aside>
        </div>
      </div>

      {showExport && (
        <div style={MODAL_OVERLAY} role="dialog" aria-modal="true" onClick={(e) => e.target === e.currentTarget && setShowExport(false)}>
          <div style={MODAL} onClick={(e) => e.stopPropagation()}>
            <div style={{ padding: '14px 18px', borderBottom: '1px solid #1e2d3d', display: 'flex', justifyContent: 'space-between' }}>
              <h2 style={{ margin: 0, fontSize: '15px' }}>SorobanAuthorizationEntry payload</h2>
              <button style={BTN_BASE} onClick={() => setShowExport(false)} aria-label="Close">✕</button>
            </div>
            <pre style={JSON_PRE}>{exportedJson}</pre>
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '8px', padding: '12px 18px', borderTop: '1px solid #1e2d3d' }}>
              <button style={BTN_BASE} onClick={handleCopy}>
                {copySuccess ? '✓ Copied!' : '⎘ Copy to Clipboard'}
              </button>
              <button style={BTN_PRIMARY} onClick={() => setShowExport(false)}>Done</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function indexNodes(roots: AuthTreeNode[]): Map<string, AuthTreeNode> {
  const map = new Map<string, AuthTreeNode>();
  const walk = (n: AuthTreeNode) => {
    map.set(n.id, n);
    n.children.forEach(walk);
  };
  roots.forEach(walk);
  return map;
}

function buildNodeFlagMap<T extends { nodeIds: string[] }>(items: T[]): Map<string, T[]> {
  const map = new Map<string, T[]>();
  for (const item of items) {
    for (const id of item.nodeIds) {
      const list = map.get(id) ?? [];
      list.push(item);
      map.set(id, list);
    }
  }
  return map;
}

export default AuthTreeBuilder;
