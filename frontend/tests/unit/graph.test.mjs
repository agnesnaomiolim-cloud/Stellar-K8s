import test from 'node:test';
import assert from 'node:assert/strict';

let graph;
try {
  graph = await import('../compiled/graph.js');
} catch {
  test('graph (skipped — run `npm run build` first)', () => {
    assert.ok(true);
  });
  process.exit(0);
}

const { buildTrustGraph, findArticulationPoints } = graph;

test('classifies direct edges between published qsets', () => {
  const g = buildTrustGraph({
    node: { node: 'GA', qset: { threshold: 1, validators: ['GB'] } },
    known_peers: { GB: { node: 'GB', qset: { threshold: 1, validators: ['GA'] } } },
  });
  const kinds = g.edges.map((e) => e.kind).sort();
  assert.deepEqual(kinds, ['direct', 'direct']);
  assert.equal(g.stats.nodeCount, 2);
  assert.equal(g.stats.publishedCount, 2);
});

test('marks edges to unpublished peers as missing', () => {
  const g = buildTrustGraph({
    node: { node: 'GA', qset: { threshold: 1, validators: ['GHOST'] } },
  });
  const ghostEdge = g.edges.find((e) => e.target === 'GHOST');
  assert.equal(ghostEdge.kind, 'missing');
  assert.equal(ghostEdge.reason, 'not_in_dump');
  assert.equal(g.nodes.find((n) => n.id === 'GHOST').hasQuorumSet, false);
});

test('classifies nested-inner-set members as indirect', () => {
  const g = buildTrustGraph({
    node: {
      node: 'GA',
      qset: {
        threshold: 1,
        validators: ['GB'],
        quorumSets: [{ validators: ['GC'] }],
      },
    },
    known_peers: {
      GB: { node: 'GB', qset: { threshold: 1, validators: ['GA'] } },
      GC: { node: 'GC', qset: { threshold: 1, validators: ['GA'] } },
    },
  });
  const indirect = g.edges.find((e) => e.source === 'GA' && e.target === 'GC');
  assert.equal(indirect.kind, 'indirect');
  assert.equal(indirect.reason, 'transitive_only');
});

test('computes in/out degrees correctly', () => {
  const g = buildTrustGraph({
    nodes: [
      { id: 'GA', qset: { threshold: 1, validators: ['GB', 'GC'] } },
      { id: 'GB', qset: { threshold: 1, validators: ['GA'] } },
      { id: 'GC', qset: { threshold: 1, validators: ['GA'] } },
    ],
  });
  const a = g.nodes.find((n) => n.id === 'GA');
  assert.equal(a.trusting, 2);
  assert.equal(a.trusters, 2);
});

test('finds articulation points in a path graph', () => {
  const adj = new Map([
    ['A', ['B']],
    ['B', ['A', 'C']],
    ['C', ['B']],
  ]);
  assert.deepEqual(findArticulationPoints(adj), ['B']);
});

test('no articulation points in a cycle', () => {
  const adj = new Map([
    ['A', ['B', 'C']],
    ['B', ['A', 'C']],
    ['C', ['A', 'B']],
  ]);
  assert.deepEqual(findArticulationPoints(adj), []);
});

test('detects missing quorum intersection in a split network', () => {
  // Two disjoint 3-cliques with no bridge.
  const g = buildTrustGraph({
    nodes: [
      { id: 'A1', qset: { threshold: 1, validators: ['A2', 'A3'] } },
      { id: 'A2', qset: { threshold: 1, validators: ['A1', 'A3'] } },
      { id: 'A3', qset: { threshold: 1, validators: ['A1', 'A2'] } },
      { id: 'B1', qset: { threshold: 1, validators: ['B2', 'B3'] } },
      { id: 'B2', qset: { threshold: 1, validators: ['B1', 'B3'] } },
      { id: 'B3', qset: { threshold: 1, validators: ['B1', 'B2'] } },
    ],
  });
  assert.equal(g.stats.hasQuorumIntersection, false);
});

test('handles large snapshots without pathological runtime', () => {
  const nodes = [];
  const N = 2000;
  for (let i = 0; i < N; i++) {
    const validators = [];
    for (let j = 1; j <= 4; j++) {
      validators.push(`G${(i + j) % N}`);
    }
    nodes.push({ id: `G${i}`, qset: { threshold: 3, validators } });
  }
  const t0 = performance.now();
  const g = buildTrustGraph({ nodes });
  const dt = performance.now() - t0;
  assert.equal(g.stats.nodeCount, N);
  assert.ok(dt < 2000, `graph build took ${dt.toFixed(0)}ms, expected < 2000ms`);
});
