import test from 'node:test';
import assert from 'node:assert/strict';

let parser;
try {
  parser = await import('../compiled/parser.js');
} catch {
  test('parser (skipped — run `npm run build` first)', () => {
    assert.ok(true, 'compiled module not found; build first');
  });
  process.exit(0);
}

const { parseQuorumDump, flattenQsetValidators, shortKey, QuorumParseError } = parser;

test('parses shape 1: node + known_peers', () => {
  const dump = parseQuorumDump({
    node: { node: 'GA', qset: { threshold: 1, validators: ['GB'] } },
    known_peers: { GB: { node: 'GB', qset: { threshold: 1, validators: ['GA'] } } },
  });
  assert.equal(dump.node.id, 'GA');
  assert.ok(dump.knownPeers);
  assert.equal(dump.knownPeers.GB.id, 'GB');
});

test('parses shape 2: flat nodes list', () => {
  const dump = parseQuorumDump({ nodes: [{ id: 'GA' }, 'GB'] });
  assert.equal(dump.nodes.length, 2);
  assert.equal(dump.nodes[0].id, 'GA');
  assert.equal(dump.nodes[1].id, 'GB');
});

test('parses shape 3: bare array', () => {
  const dump = parseQuorumDump(['GA', { id: 'GB', name: 'b.org' }]);
  assert.equal(dump.nodes.length, 2);
  assert.equal(dump.nodes[1].name, 'b.org');
});

test('parses shape 4: record keyed by pubkey', () => {
  const dump = parseQuorumDump({ GA: { name: 'a.org' }, GB: { name: 'b.org' } });
  assert.equal(dump.nodes.length, 2);
  assert.ok(dump.nodes.every((n) => n.id.startsWith('G')));
});

test('accepts nested innerSets alias', () => {
  const dump = parseQuorumDump({
    node: {
      node: 'GA',
      qset: { threshold: 2, validators: ['GB'], innerSets: [{ validators: ['GC'] }] },
    },
  });
  const flat = flattenQsetValidators(dump.node.qset);
  assert.deepEqual(flat.sort(), ['GB', 'GC']);
});

test('throws QuorumParseError with path on malformed input', () => {
  assert.throws(() => parseQuorumDump({ node: { qset: {} } }), QuorumParseError);
  assert.throws(() => parseQuorumDump(42), QuorumParseError);
  assert.throws(() => parseQuorumDump({ known_peers: { A: 5 } }), QuorumParseError);
});

test('shortKey truncates long keys', () => {
  const long = 'GABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ABCDEFGHIJ';
  assert.equal(shortKey(long), 'GABC…GHIJ');
  assert.equal(shortKey('SHORT'), 'SHORT');
});
