/**
 * Auth Tree Store — React context + reducer based state management for the
 * Soroban Auth Hierarchy Builder canvas.
 *
 * Usage:
 *   <AuthTreeProvider>
 *     <AuthTreeBuilder />
 *   </AuthTreeProvider>
 *
 *   const [state, dispatch] = useAuthTree();
 *   dispatch({ type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice', address: 'GABC...' } });
 */

import React, { createContext, useContext, useReducer } from 'react';
import type { AuthArg, AuthNodeKind, AuthTreeNode } from '../../../utils/auth_validator';

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

export interface AuthTreeState {
  roots: AuthTreeNode[];
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

export interface AddRootAddressPayload {
  id: string;
  address?: string;
}

export interface AddChildPayload {
  /** ID of the parent node the new node will be nested under. */
  parentId: string;
  /** Newly generated ID for the child node. */
  id: string;
  kind: AuthNodeKind;
}

export interface RemoveNodePayload {
  nodeId: string;
}

export interface UpdateNodePayload {
  nodeId: string;
  updates: Partial<Pick<AuthTreeNode, 'address' | 'contractAddress' | 'functionName' | 'args'>>;
}

export interface ReparentNodePayload {
  /** ID of the node being moved. */
  nodeId: string;
  /** ID of the new parent node. Reparenting onto a descendant of itself is a no-op. */
  newParentId: string;
}

export interface SetArgsPayload {
  nodeId: string;
  args: AuthArg[];
}

export type AuthTreeAction =
  | { type: 'ADD_ROOT_ADDRESS'; payload: AddRootAddressPayload }
  | { type: 'ADD_CHILD'; payload: AddChildPayload }
  | { type: 'REMOVE_NODE'; payload: RemoveNodePayload }
  | { type: 'UPDATE_NODE'; payload: UpdateNodePayload }
  | { type: 'REPARENT_NODE'; payload: ReparentNodePayload }
  | { type: 'SET_ARGS'; payload: SetArgsPayload }
  | { type: 'RESET' };

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Generates a lightweight pseudo-UUID for client-side node IDs. */
export function generateNodeId(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

/** Returns a new tree with `updater` applied to the node matching `nodeId`, wherever it is nested. */
function mapNode(
  nodes: AuthTreeNode[],
  nodeId: string,
  updater: (node: AuthTreeNode) => AuthTreeNode,
): AuthTreeNode[] {
  return nodes.map((n) => {
    if (n.id === nodeId) return updater(n);
    if (n.children.length === 0) return n;
    return { ...n, children: mapNode(n.children, nodeId, updater) };
  });
}

/** Removes the node matching `nodeId` from anywhere in the tree. */
function removeNode(nodes: AuthTreeNode[], nodeId: string): AuthTreeNode[] {
  return nodes
    .filter((n) => n.id !== nodeId)
    .map((n) => (n.children.length === 0 ? n : { ...n, children: removeNode(n.children, nodeId) }));
}

/** True if `candidateId` is `ancestorId` itself or nested anywhere beneath it. */
function isSelfOrDescendant(node: AuthTreeNode, candidateId: string): boolean {
  if (node.id === candidateId) return true;
  return node.children.some((c) => isSelfOrDescendant(c, candidateId));
}

function findNode(nodes: AuthTreeNode[], nodeId: string): AuthTreeNode | null {
  for (const n of nodes) {
    if (n.id === nodeId) return n;
    const found = findNode(n.children, nodeId);
    if (found) return found;
  }
  return null;
}

// ---------------------------------------------------------------------------
// Reducer
// ---------------------------------------------------------------------------

export function authTreeReducer(state: AuthTreeState, action: AuthTreeAction): AuthTreeState {
  switch (action.type) {
    case 'ADD_ROOT_ADDRESS': {
      const { id, address } = action.payload;
      if (state.roots.some((r) => r.id === id)) return state;
      const node: AuthTreeNode = { id, kind: 'address', address, children: [] };
      return { ...state, roots: [...state.roots, node] };
    }

    case 'ADD_CHILD': {
      const { parentId, id, kind } = action.payload;
      const child: AuthTreeNode = { id, kind, children: [] };
      return {
        ...state,
        roots: mapNode(state.roots, parentId, (parent) => ({
          ...parent,
          children: [...parent.children, child],
        })),
      };
    }

    case 'REMOVE_NODE': {
      return { ...state, roots: removeNode(state.roots, action.payload.nodeId) };
    }

    case 'UPDATE_NODE': {
      const { nodeId, updates } = action.payload;
      return { ...state, roots: mapNode(state.roots, nodeId, (n) => ({ ...n, ...updates })) };
    }

    case 'SET_ARGS': {
      const { nodeId, args } = action.payload;
      return { ...state, roots: mapNode(state.roots, nodeId, (n) => ({ ...n, args })) };
    }

    case 'REPARENT_NODE': {
      const { nodeId, newParentId } = action.payload;
      if (nodeId === newParentId) return state;

      const moved = findNode(state.roots, nodeId);
      if (!moved) return state;
      // Refuse to reparent onto self or one of its own descendants (would create a cycle).
      if (isSelfOrDescendant(moved, newParentId)) return state;
      // Address nodes may never be reparented — they are always tree roots.
      if (moved.kind === 'address') return state;

      const withoutMoved = removeNode(state.roots, nodeId);
      return {
        ...state,
        roots: mapNode(withoutMoved, newParentId, (parent) => ({
          ...parent,
          children: [...parent.children, moved],
        })),
      };
    }

    case 'RESET': {
      return createInitialState();
    }

    default: {
      const _exhaustive: never = action;
      return _exhaustive;
    }
  }
}

// ---------------------------------------------------------------------------
// Initial state
// ---------------------------------------------------------------------------

export function createInitialState(): AuthTreeState {
  return { roots: [] };
}

// ---------------------------------------------------------------------------
// React context
// ---------------------------------------------------------------------------

type AuthTreeContextValue = [AuthTreeState, React.Dispatch<AuthTreeAction>];

export const AuthTreeContext = createContext<AuthTreeContextValue | undefined>(undefined);

export interface AuthTreeProviderProps {
  children: React.ReactNode;
  initialState?: AuthTreeState;
}

export function AuthTreeProvider({
  children,
  initialState,
}: AuthTreeProviderProps): React.JSX.Element {
  const [state, dispatch] = useReducer(authTreeReducer, initialState ?? createInitialState());
  return React.createElement(AuthTreeContext.Provider, { value: [state, dispatch] }, children);
}

export function useAuthTree(): AuthTreeContextValue {
  const ctx = useContext(AuthTreeContext);
  if (ctx === undefined) {
    throw new Error(
      'useAuthTree must be called inside an <AuthTreeProvider>. ' +
        'Make sure your component tree is wrapped with <AuthTreeProvider>.',
    );
  }
  return ctx;
}
