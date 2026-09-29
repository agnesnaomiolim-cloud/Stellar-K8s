import type { QuorumDump, QuorumSetEntry, QuorumSetLike } from './types.js';

/**
 * Parse stellar-core quorum set JSON dumps into a normalized
 * `QuorumDump` structure.
 *
 * Supported shapes:
 *  1. `{ node: {...}, known_peers: {...} }` — core dump file
 *  2. `{ nodes: [...] }`                    — flat `/quorum` style list
 *  3. Bare arrays of node entries           — simple exports
 *  4. Nested `validators` arrays containing objects with `qset`
 *
 * Unknown or malformed payloads throw `QuorumParseError` with the
 * JSON path of the offending element, so callers can surface
 * actionable diagnostics instead of silently rendering nothing.
 */
export class QuorumParseError extends Error {
  constructor(
    message: string,
    public readonly path: string,
  ) {
    super(`${message} (at ${path})`);
    this.name = 'QuorumParseError';
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function asString(value: unknown, path: string): string {
  if (typeof value !== 'string' || value.length === 0) {
    throw new QuorumParseError('expected non-empty string', path);
  }
  return value;
}

function asEntry(value: unknown, path: string): QuorumSetEntry {
  if (typeof value === 'string') {
    return { id: value };
  }
  if (!isRecord(value)) {
    throw new QuorumParseError('expected node id string or object', path);
  }
  const id = asString(value['node'] ?? value['id'], `${path}.id`);
  const name =
    typeof value['name'] === 'string' && value['name'].length > 0
      ? value['name']
      : typeof value['domain'] === 'string' && (value['domain'] as string).length > 0
        ? (value['domain'] as string)
        : undefined;
  const qset = isRecord(value['qset']) ? (value['qset'] as QuorumSetLike) : undefined;
  return qset ? { id, name, qset } : name ? { id, name } : { id };
}

/** Normalize any of the supported dump shapes into a `QuorumDump`. */
export function parseQuorumDump(raw: unknown): QuorumDump {
  if (Array.isArray(raw)) {
    return { nodes: raw.map((e, i) => asEntry(e, `nodes[${i}]`)) };
  }
  if (!isRecord(raw)) {
    throw new QuorumParseError('expected object or array', '$');
  }
  // Shape 1: { node, known_peers } or { node, knownPeers }
  if (isRecord(raw['node'])) {
    const node = asEntry(raw['node'], 'node');
    const rawPeers = raw['known_peers'] ?? raw['knownPeers'];
    if (rawPeers === undefined) {
      return { node };
    }
    if (!isRecord(rawPeers)) {
      throw new QuorumParseError('expected object keyed by public key', 'known_peers');
    }
    const knownPeers: Record<string, QuorumSetEntry> = {};
    for (const [key, value] of Object.entries(rawPeers)) {
      knownPeers[key] = asEntry(value, `known_peers["${key}"]`);
    }
    return { node, knownPeers };
  }
  // Shape 2: { nodes: [...] }
  if (Array.isArray(raw['nodes'])) {
    return { nodes: (raw['nodes'] as unknown[]).map((e, i) => asEntry(e, `nodes[${i}]`)) };
  }
  // Shape 3: bare map keyed by public key (no reserved top-level keys).
  const RESERVED = new Set(['node', 'known_peers', 'knownPeers', 'nodes']);
  const keys = Object.keys(raw);
  const hasReservedKey = keys.some((k) => RESERVED.has(k));
  if (!hasReservedKey && keys.length > 0 && keys.every((k) => isRecord(raw[k]))) {
    const nodes = keys.map((k) => {
      const value = raw[k] as Record<string, unknown>;
      return asEntry({ ...value, id: k }, `"${k}"`);
    });
    return { nodes };
  }
  throw new QuorumParseError('unrecognized quorum dump shape', '$');
}

/** Extract the full recursive validator list from a quorum set. */
export function flattenQsetValidators(qset: QuorumSetLike | null | undefined): string[] {
  if (!qset) return [];
  const out: string[] = [];
  const visit = (set: QuorumSetLike, depth: number): void => {
    if (depth > 32) return; // guard against pathological nesting
    for (const v of set.validators ?? []) {
      out.push(typeof v === 'string' ? v : v.id);
    }
    for (const nested of set.quorumSets ?? set.innerSets ?? []) {
      visit(nested, depth + 1);
    }
  };
  visit(qset, 0);
  return out;
}

/** Shorten a public key for display. */
export function shortKey(key: string): string {
  return key.length <= 12 ? key : `${key.slice(0, 4)}…${key.slice(-4)}`;
}
