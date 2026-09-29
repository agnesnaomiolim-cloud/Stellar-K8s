#!/usr/bin/env node
/**
 * Generate a mainnet-scale synthetic quorum snapshot.
 *
 * The structure mirrors real stellar-core dumps: a handful of
 * well-connected organizations each running several validators, with
 * nested inner quorum sets, plus long-tail independent validators that
 * publish no qset of their own (the "missing peers" case).
 *
 * Usage: node scripts/generate_snapshot.mjs [output] [scale]
 *   scale 1 ≈ today's mainnet (~80 published, ~500 total nodes)
 *   scale 4 ≈ stress target (2,000 nodes)
 */
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDir = dirname(fileURLToPath(import.meta.url));
const outArg =
  process.argv[2] ?? resolve(scriptDir, '../public/snapshots/quorum_snapshot.json');
const scale = Number(process.argv[3] ?? 1);
// Resolve the output path from the current working directory (falling back
// to script-relative only for absolute defaults) so `npm run` invocations
// from frontend/ work with plain relative paths like `public/…`.
const outPath = resolve(outArg);

const KEYCHARS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ2-7'; // stellar base32-ish

function fakeKey(i) {
  let n = i;
  const chars = [];
  for (let c = 0; c < 56; c++) {
    chars.push(KEYCHARS[n % KEYCHARS.length]);
    n = Math.floor(n / KEYCHARS.length) + 7 * (c + 1) * (i + 3);
  }
  return `G${chars.join('')}`;
}

// Deterministic PRNG (mulberry32) for reproducible snapshots.
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const rand = mulberry32(31337);

const ORGS = [
  'sdf', 'coinbase', 'kraken', 'binance', 'bitgo', 'satoshipay', 'wirex',
  'coinqvest', 'lobstr', ' stellar', 'anchor', 'fchain', 'tempo', 'ibereval',
  'whale', 'trustline',
];

const nodes = [];
let keyIndex = 0;

function nextKey() {
  return fakeKey(keyIndex++);
}

// ── Tier 1: full orgs with nested inner sets ─────────────────
const orgValidators = [];
for (const org of ORGS) {
  const count = 2 + Math.floor(rand() * 3); // 2-4 validators each
  const members = [];
  for (let i = 0; i < count; i++) {
    const key = nextKey();
    members.push(key);
    nodes.push({ id: key, name: `${org}-v${i + 1}.stellar.org` });
  }
  orgValidators.push({ org, members });
}

// ── Tier 2: independent validators with published qsets ──────
const independents = [];
const indepCount = Math.floor(18 * scale);
for (let i = 0; i < indepCount; i++) {
  const key = nextKey();
  independents.push(key);
  nodes.push({ id: key, name: `independent-${i + 1}.stellarops.io` });
}

// ── Tier 3: long-tail silent validators (never publish) ──────
const silentCount = Math.floor(420 * scale);
const silent = [];
for (let i = 0; i < silentCount; i++) {
  const key = nextKey();
  silent.push(key);
  nodes.push({ id: key }); // no name, no qset
}

// ── Build quorum sets ────────────────────────────────────────
// Each org's qset: 67% threshold on its own validators + 2 inner sets
// containing a sample of other orgs' validators (overlapping slices).
function pickFrom(pool, n) {
  const out = [];
  const copy = [...pool];
  for (let i = 0; i < n && copy.length > 0; i++) {
    const idx = Math.floor(rand() * copy.length);
    out.push(copy.splice(idx, 1)[0]);
  }
  return out;
}

const allPublished = [...orgValidators.flatMap((o) => o.members), ...independents];

function qsetFor(members) {
  return {
    threshold: Math.max(2, Math.ceil(members.length * 0.67)),
    validators: members,
  };
}

const dump = {};
const orgEntries = {};
for (const { org, members } of orgValidators) {
  const innerSets = [];
  const others = allPublished.filter((k) => !members.includes(k));
  for (let s = 0; s < 2; s++) {
    innerSets.push(qsetFor(pickFrom(others, 3 + Math.floor(rand() * 3))));
  }
  const entry = {
    threshold: Math.max(2, Math.ceil((members.length + innerSets.length) * 0.67)),
    validators: members,
    quorumSets: innerSets,
  };
  orgEntries[org] = entry;
}

// Shape 1 dump: node + known_peers (matches stellar-core `--dump-quorum`)
const firstOrg = orgValidators[0];
dump.node = {
  node: firstOrg.members[0],
  name: `${firstOrg.org}-v1.stellar.org`,
  qset: orgEntries[firstOrg.org],
};
dump.known_peers = {};
for (const { org, members } of orgValidators) {
  members.forEach((key, i) => {
    dump.known_peers[key] = {
      node: key,
      name: `${org}-v${i + 1}.stellar.org`,
      qset: structuredClone(orgEntries[org]),
    };
  });
}
for (const key of independents) {
  const trusted = pickFrom(allPublished, 4 + Math.floor(rand() * 4));
  dump.known_peers[key] = {
    node: key,
    name: undefined,
    qset: {
      threshold: Math.max(2, Math.ceil(trusted.length * 0.7)),
      validators: trusted,
    },
  };
}
// Silent validators: a few per org get referenced inside that org's inner
// sets without being published themselves (realistic observability gaps →
// 'missing' edges in the visualizer). The rest appear only in the peers
// table, known to the network but invisible to the trust graph.
const orgKeys = orgValidators.flatMap((o) => o.members);
const refsPerOrg = 3;
for (let i = 0; i < silent.length; i++) {
  const key = silent[i];
  const orgSlot = Math.floor(i / refsPerOrg);
  if (orgSlot < orgValidators.length) {
    const host = orgKeys[orgSlot % orgKeys.length];
    const inner = dump.known_peers[host].qset.quorumSets[orgSlot % 2];
    inner.validators.push(key);
    // Threshold intentionally unchanged: the member is listed but the set
    // has not re-signed its policy, exactly the drift the visualizer flags.
  } else {
    dump.known_peers[key] = { node: key };
  }
}

mkdirSync(dirname(outPath), { recursive: true });
writeFileSync(outPath, JSON.stringify(dump, null, 1));

const totalNodes =
  nodes.length + 0; // info only
const edgeEstimate =
  orgValidators.reduce((acc, o) => acc + o.members.length * (o.members.length - 1 + 6), 0) +
  independents.reduce((acc) => acc + 6, 0) +
  silent.length;
console.log(
  `snapshot written: ${outPath}`,
  `\n  scale=${scale}`,
  `\n  org validators: ${orgValidators.reduce((a, o) => a + o.members.length, 0)}`,
  `\n  independents:   ${independents.length}`,
  `\n  silent:         ${silent.length}`,
  `\n  total nodes:    ~${totalNodes}`,
  `\n  edge estimate:  ~${edgeEstimate}`,
);
