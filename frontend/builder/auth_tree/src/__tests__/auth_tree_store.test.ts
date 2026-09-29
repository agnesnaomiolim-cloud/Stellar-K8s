/**
 * Vitest unit tests for the auth_tree_store reducer.
 */

import { describe, it, expect } from 'vitest';
import { authTreeReducer, createInitialState } from '../auth_tree_store';

describe('authTreeReducer — ADD_ROOT_ADDRESS', () => {
  it('adds a new address root', () => {
    const state = authTreeReducer(createInitialState(), {
      type: 'ADD_ROOT_ADDRESS',
      payload: { id: 'alice', address: 'GALICE' },
    });
    expect(state.roots).toHaveLength(1);
    expect(state.roots[0]).toMatchObject({ id: 'alice', kind: 'address', address: 'GALICE', children: [] });
  });

  it('does not add a duplicate id', () => {
    let state = authTreeReducer(createInitialState(), {
      type: 'ADD_ROOT_ADDRESS',
      payload: { id: 'alice', address: 'GALICE' },
    });
    state = authTreeReducer(state, { type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice', address: 'GOTHER' } });
    expect(state.roots).toHaveLength(1);
    expect(state.roots[0].address).toBe('GALICE');
  });
});

describe('authTreeReducer — ADD_CHILD', () => {
  it('nests a contract call under an address root', () => {
    let state = authTreeReducer(createInitialState(), {
      type: 'ADD_ROOT_ADDRESS',
      payload: { id: 'alice', address: 'GALICE' },
    });
    state = authTreeReducer(state, {
      type: 'ADD_CHILD',
      payload: { parentId: 'alice', id: 'router', kind: 'contract' },
    });
    expect(state.roots[0].children).toHaveLength(1);
    expect(state.roots[0].children[0]).toMatchObject({ id: 'router', kind: 'contract', children: [] });
  });

  it('nests a sub-invocation under an existing contract node', () => {
    let state = authTreeReducer(createInitialState(), {
      type: 'ADD_ROOT_ADDRESS',
      payload: { id: 'alice' },
    });
    state = authTreeReducer(state, {
      type: 'ADD_CHILD',
      payload: { parentId: 'alice', id: 'router', kind: 'contract' },
    });
    state = authTreeReducer(state, {
      type: 'ADD_CHILD',
      payload: { parentId: 'router', id: 'pool', kind: 'contract' },
    });
    expect(state.roots[0].children[0].children[0].id).toBe('pool');
  });
});

describe('authTreeReducer — UPDATE_NODE / SET_ARGS', () => {
  it('updates fields on a nested node', () => {
    let state = authTreeReducer(createInitialState(), { type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'alice', id: 'router', kind: 'contract' } });
    state = authTreeReducer(state, {
      type: 'UPDATE_NODE',
      payload: { nodeId: 'router', updates: { contractAddress: 'CROUTER', functionName: 'swap' } },
    });
    expect(state.roots[0].children[0]).toMatchObject({ contractAddress: 'CROUTER', functionName: 'swap' });
  });

  it('sets args on a node', () => {
    let state = authTreeReducer(createInitialState(), { type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'alice', id: 'router', kind: 'contract' } });
    state = authTreeReducer(state, {
      type: 'SET_ARGS',
      payload: { nodeId: 'router', args: [{ name: 'amount', type: 'i128', value: '100' }] },
    });
    expect(state.roots[0].children[0].args).toEqual([{ name: 'amount', type: 'i128', value: '100' }]);
  });
});

describe('authTreeReducer — REMOVE_NODE', () => {
  it('removes a nested node and its descendants', () => {
    let state = authTreeReducer(createInitialState(), { type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'alice', id: 'router', kind: 'contract' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'router', id: 'pool', kind: 'contract' } });
    state = authTreeReducer(state, { type: 'REMOVE_NODE', payload: { nodeId: 'router' } });
    expect(state.roots[0].children).toHaveLength(0);
  });
});

describe('authTreeReducer — REPARENT_NODE', () => {
  it('moves a contract node under a new parent', () => {
    let state = authTreeReducer(createInitialState(), { type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'alice', id: 'router', kind: 'contract' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'alice', id: 'pool', kind: 'contract' } });
    state = authTreeReducer(state, { type: 'REPARENT_NODE', payload: { nodeId: 'pool', newParentId: 'router' } });

    expect(state.roots[0].children).toHaveLength(1);
    expect(state.roots[0].children[0].id).toBe('router');
    expect(state.roots[0].children[0].children[0].id).toBe('pool');
  });

  it('refuses to reparent a node onto its own descendant (would create a cycle)', () => {
    let state = authTreeReducer(createInitialState(), { type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'alice', id: 'router', kind: 'contract' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'router', id: 'pool', kind: 'contract' } });

    const before = state;
    const after = authTreeReducer(state, { type: 'REPARENT_NODE', payload: { nodeId: 'router', newParentId: 'pool' } });
    expect(after).toBe(before);
  });

  it('refuses to reparent an address node', () => {
    let state = authTreeReducer(createInitialState(), { type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice' } });
    state = authTreeReducer(state, { type: 'ADD_ROOT_ADDRESS', payload: { id: 'bob' } });
    state = authTreeReducer(state, { type: 'ADD_CHILD', payload: { parentId: 'bob', id: 'router', kind: 'contract' } });

    const after = authTreeReducer(state, { type: 'REPARENT_NODE', payload: { nodeId: 'alice', newParentId: 'router' } });
    expect(after.roots).toHaveLength(2);
  });
});

describe('authTreeReducer — RESET', () => {
  it('clears all roots', () => {
    let state = authTreeReducer(createInitialState(), { type: 'ADD_ROOT_ADDRESS', payload: { id: 'alice' } });
    state = authTreeReducer(state, { type: 'RESET' });
    expect(state.roots).toHaveLength(0);
  });
});
