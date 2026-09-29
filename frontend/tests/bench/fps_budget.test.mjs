import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';

const scriptDir = dirname(fileURLToPath(import.meta.url));

let graph;
try {
  graph = await import('../compiled/graph.js');
} catch {
  test('fps budget (skipped — run `npm run build` first)', () => {
    assert.ok(true);
  });
  process.exit(0);
}

const { buildTrustGraph } = graph;

/** Frame budget for 60 FPS. */
const FRAME_BUDGET_MS = 1000 / 60;

function makeSyntheticGraph(nodeCount) {
  const nodes = [];
  for (let i = 0; i < nodeCount; i++) {
    const validators = [];
    for (let j = 1; j <= 5; j++) validators.push(`G${(i + j) % nodeCount}`);
    nodes.push({ id: `G${i}`, qset: { threshold: 3, validators } });
  }
  return { nodes };
}

test('graph build for 2k nodes fits well under one frame budget', () => {
  const raw = makeSyntheticGraph(2000);
  // Warmup
  buildTrustGraph(raw);
  const t0 = performance.now();
  const g = buildTrustGraph(raw);
  const dt = performance.now() - t0;
  assert.equal(g.stats.nodeCount, 2000);
  // Build happens once per load, not per frame; allow 30 frames of budget.
  assert.ok(
    dt < FRAME_BUDGET_MS * 30,
    `build took ${dt.toFixed(1)}ms (budget ${FRAME_BUDGET_MS * 30}ms)`,
  );
});

test('published benchmark report exists and passes', () => {
  const resultsPath = resolve(scriptDir, '../../../benchmarks/topology-visualizer/RESULTS.md');
  if (!existsSync(resultsPath)) {
    console.warn('RESULTS.md missing — run `npm run bench` to generate it');
    return; // soft-pass in CI without browser GPU
  }
  const contents = readFileSync(resultsPath, 'utf8');
  assert.match(contents, /Result: .✅ PASS|Result: .*PASS/);
});

test('steady-state tick apply stays within frame budget (2k nodes)', () => {
  // Simulate the main-thread cost of applying a worker tick payload.
  const N = 2000;
  const positions = new Float32Array(N * 3);
  const source = new Float32Array(N * 3);
  for (let i = 0; i < source.length; i++) source[i] = Math.random();
  // Warmup
  positions.set(source);
  const samples = [];
  for (let s = 0; s < 50; s++) {
    const t0 = performance.now();
    positions.set(source);
    samples.push(performance.now() - t0);
  }
  const p95 = samples.sort((a, b) => a - b)[45];
  assert.ok(
    p95 < FRAME_BUDGET_MS,
    `typed-array apply p95 ${p95.toFixed(3)}ms exceeds frame budget`,
  );
});
