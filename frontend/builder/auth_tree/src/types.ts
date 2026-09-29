/**
 * UI-local type definitions for the Auth Hierarchy Builder canvas.
 *
 * Domain types for the tree itself (`AuthTreeNode`, `ValidationResult`, the
 * JSON payload shapes) live in `frontend/utils/auth_validator.ts` — this
 * module only adds the drag-and-drop / canvas-specific types the React
 * components need.
 */

import type { AuthNodeKind } from '../../../utils/auth_validator';

/**
 * Data transferred during a drag-and-drop operation on the auth tree canvas.
 *
 * Two drag sources exist:
 * - `palette`: dragging a new node kind ('address' or 'contract') from the
 *   palette onto the canvas (root drop) or onto an existing node (nested drop).
 * - `existing-node`: dragging an already-placed node onto a new parent to
 *   reparent it.
 */
export interface DragPayload {
  type: 'palette' | 'existing-node';
  /** The node kind being created; always populated for palette drags. */
  kind: AuthNodeKind;
  /** ID of the node being moved when `type === 'existing-node'`; null otherwise. */
  nodeId: string | null;
}

/** Computed screen position for one node, used to draw the canvas layout. */
export interface LayoutPosition {
  id: string;
  depth: number;
  x: number;
  y: number;
}

/** Pan/zoom transform applied to the canvas "world" container. */
export interface CanvasTransform {
  x: number;
  y: number;
  scale: number;
}

export const DEFAULT_TRANSFORM: CanvasTransform = { x: 40, y: 40, scale: 1 };
export const MIN_SCALE = 0.4;
export const MAX_SCALE = 1.75;
