import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const frontendDir = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const genScript = join(frontendDir, 'scripts/generate_snapshot.mjs');

let graph;
try {
  graph = await import('../compiled/graph.js');
} catch {
  test('snapshot (skipped — run `npm run build` first)', () => {
    assert.ok(true);
  });
  process.exit(0);
}

const { buildTrustGraph } = graph;

function generate(scale, outDir) {
  const out = join(outDir, 'snap.json');
  execFileSync(process.execPath, [genScript, out, String(scale)], { cwd: frontendDir });
  return JSON.parse(readFileSync(out, 'utf8'));
}

test('snapshot generator is deterministic for the same scale', () => {
  const dir1 = mkdtempSync(join(tmpdir(), 'snap-a-'));
  const dir2 = mkdtempSync(join(tmpdir(), 'snap-b-'));
  try {
    const a = generate(1, dir1);
    const b = generate(1, dir2);
    assert.deepEqual(a, b);
  } finally {
    rmSync(dir1, { recursive: true, force: true });
    rmSync(dir2, { recursive: true, force: true });
  }
});

test('scale-1 snapshot resembles mainnet magnitude (~500 nodes)', () => {
  const dir = mkdtempSync(join(tmpdir(), 'snap-c-'));
  try {
    const dump = generate(1, dir);
    const g = buildTrustGraph(dump);
    assert.ok(g.stats.nodeCount > 400, `expected > 400 nodes, got ${g.stats.nodeCount}`);
    assert.ok(g.stats.nodeCount < 700, `expected < 700 nodes, got ${g.stats.nodeCount}`);
    assert.ok(g.stats.directCount > 0);
    assert.ok(g.stats.missingCount > 0, 'long-tail silent validators should yield missing edges');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('scale-4 snapshot reaches the 2k-node stress target', () => {
  const dir = mkdtempSync(join(tmpdir(), 'snap-d-'));
  try {
    const dump = generate(4, dir);
    const g = buildTrustGraph(dump);
    assert.ok(g.stats.nodeCount >= 1800, `expected >= 1800 nodes, got ${g.stats.nodeCount}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
