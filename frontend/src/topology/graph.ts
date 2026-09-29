import type {
  GraphEdge,
  GraphNode,
  GraphStats,
  QuorumDump,
  QuorumGraph,
  QuorumSetEntry,
  TrustKind,
} from './types.js';
import { parseQuorumDump, shortKey } from './parser.js';

/**
 * Build a directed trust graph from a quorum dump.
 *
 * Edge classification mirrors operator intuition:
 *  - `direct`   — the trusted node appears in the trusting node's quorum set
 *                 (i.e. both endpoints are known to the dump and the
 *                 trusting node published a qset that includes the target).
 *  - `indirect` — trust is only implied through nested quorum sets of
 *                 intermediate nodes (transitive, weaker signal).
 *  - `missing`  — the trusted node never published its own quorum set to
 *                 the network, so it is a potential observability gap, or
 *                 the edge only exists via another node's transitive view.
 *
 * The builder also computes:
 *  - in/out degree per node (`trusters` / `trusting`)
 *  - articulation points via iterative Tarjan on the undirected projection,
 *    which approximates single-points-of-failure in trust propagation.
 *  - a quorum-intersection heuristic: two "large" quorums whose validator
 *    sets do not overlap indicate a fragmented network.
 */
export function buildTrustGraph(raw: unknown): QuorumGraph {
  const dump: QuorumDump = parseQuorumDump(raw);

  // ── 1. Collect entries ────────────────────────────────────
  const entries: QuorumSetEntry[] = [];
  if (dump.node) entries.push(dump.node);
  if (dump.knownPeers) entries.push(...Object.values(dump.knownPeers));
  if (dump.nodes) entries.push(...dump.nodes);

  if (entries.length === 0) {
    throw new Error('quorum dump contains no nodes');
  }

  // ── 2. Direct adjacency (out-edges from published qsets) ──
  /** node -> set of nodes it directly trusts (leaf or nested). */
  const direct = new Map<string, Set<string>>();
  /**
   * node -> set of nodes reachable only through nested inner sets
   * of other members. These represent second-hand trust.
   */
  const transitive = new Map<string, Set<string>>();

  for (const entry of entries) {
    if (!entry.qset) continue;
    const own = new Set<string>();
    const inner = new Set<string>();
    collectQset(entry.qset, own, inner, 0);
    own.delete(entry.id);
    inner.delete(entry.id);
    for (const t of own) inner.delete(t);
    direct.set(entry.id, own);
    if (inner.size > 0) transitive.set(entry.id, inner);
  }

  // ── 3. Nodes ───────────────────────────────────────────────
  const nodeIds = new Set<string>();
  for (const entry of entries) nodeIds.add(entry.id);
  for (const targets of direct.values()) {
    for (const t of targets) nodeIds.add(t);
  }
  for (const targets of transitive.values()) {
    for (const t of targets) nodeIds.add(t);
  }

  const inDegree = new Map<string, number>();
  const outDegree = new Map<string, number>();
  for (const id of nodeIds) {
    inDegree.set(id, 0);
    outDegree.set(id, 0);
  }
  for (const [src, targets] of direct) {
    outDegree.set(src, targets.size);
    for (const t of targets) {
      inDegree.set(t, (inDegree.get(t) ?? 0) + 1);
    }
  }

  const entryById = new Map<string, QuorumSetEntry>();
  for (const entry of entries) {
    if (!entryById.has(entry.id)) entryById.set(entry.id, entry);
  }

  const nodes: GraphNode[] = [...nodeIds].sort().map((id) => {
    const entry = entryById.get(id);
    return {
      id,
      label: entry?.name ?? shortKey(id),
      domain: entry?.name,
      hasQuorumSet: direct.has(id),
      trusters: inDegree.get(id) ?? 0,
      trusting: outDegree.get(id) ?? 0,
      isArticulationPoint: false,
    };
  });
  const mutableById = new Map(nodes.map((n) => [n.id, { ...n }]));

  // ── 4. Edges ───────────────────────────────────────────────
  const edges: GraphEdge[] = [];
  const seen = new Set<string>();
  const push = (source: string, target: string, kind: TrustKind, reason?: string): void => {
    const key = `${source}\u0000${target}`;
    if (seen.has(key)) return;
    seen.add(key);
    edges.push(
      reason
        ? { source, target, kind, reason: reason as GraphEdge['reason'] }
        : { source, target, kind },
    );
  };

  for (const [src, targets] of direct) {
    for (const t of targets) {
      // If the target publishes its own qset, this is a fully-observable
      // direct trust relationship. Otherwise the peer's trust configuration
      // is missing from the dump — an observability gap worth flagging.
      if (direct.has(t)) {
        push(src, t, 'direct');
      } else {
        push(src, t, 'missing', 'not_in_dump');
      }
    }
  }
  for (const [src, targets] of transitive) {
    for (const t of targets) {
      if (direct.get(src)?.has(t)) continue;
      // Targets reachable only through nested inner sets carry transitive
      // trust — unless the target itself publishes nothing, in which case
      // the relationship is missing information rather than weak trust.
      if (direct.has(t)) {
        push(src, t, 'indirect', 'transitive_only');
      } else {
        push(src, t, 'missing', 'not_in_dump');
      }
    }
  }

  // ── 5. Articulation points (undirected projection, iterative Tarjan)
  const adj = new Map<string, string[]>();
  for (const e of edges) {
    if (!adj.has(e.source)) adj.set(e.source, []);
    if (!adj.has(e.target)) adj.set(e.target, []);
    adj.get(e.source)!.push(e.target);
    adj.get(e.target)!.push(e.source);
  }
  const articulation = findArticulationPoints(adj);
  for (const id of articulation) {
    const n = mutableById.get(id);
    if (n) n.isArticulationPoint = true;
  }

  // ── 6. Quorum-intersection heuristic ───────────────────────
  const hasQuorumIntersection = checkQuorumIntersection(nodes, direct);

  const stats: GraphStats = {
    nodeCount: nodes.length,
    edgeCount: edges.length,
    directCount: edges.filter((e) => e.kind === 'direct').length,
    indirectCount: edges.filter((e) => e.kind === 'indirect').length,
    missingCount: edges.filter((e) => e.kind === 'missing').length,
    publishedCount: direct.size,
    articulationPoints: articulation,
    hasQuorumIntersection,
  };

  return { nodes, edges, stats };
}

function collectQset(
  qset: NonNullable<QuorumSetEntry['qset']>,
  own: Set<string>,
  inner: Set<string>,
  depth: number,
): void {
  if (depth > 32) return;
  for (const v of qset.validators ?? []) {
    const id = typeof v === 'string' ? v : v.id;
    own.add(id);
  }
  for (const nested of qset.quorumSets ?? qset.innerSets ?? []) {
    const innerOwn = new Set<string>();
    const deeperInner = new Set<string>();
    collectQset(nested, innerOwn, deeperInner, depth + 1);
    for (const id of innerOwn) inner.add(id);
    for (const id of deeperInner) inner.add(id);
  }
}

/**
 * Iterative Tarjan articulation-point detection. Returns the set of
 * node ids whose removal increases the number of connected components
 * in the undirected projection of the trust graph.
 */
export function findArticulationPoints(adj: Map<string, string[]>): string[] {
  const result: string[] = [];
  const disc = new Map<string, number>();
  const low = new Map<string, number>();
  let timer = 0;

  for (const start of adj.keys()) {
    if (disc.has(start)) continue;
    const rootChildren = new Set<string>();
    // Frame: [node, parent, childIndex]
    const stack: Array<{ node: string; parent: string | null; ci: number }> = [
      { node: start, parent: null, ci: 0 },
    ];
    disc.set(start, timer);
    low.set(start, timer);
    timer += 1;

    while (stack.length > 0) {
      const frame = stack[stack.length - 1]!;
      const neighbors = adj.get(frame.node) ?? [];
      if (frame.ci < neighbors.length) {
        const next = neighbors[frame.ci]!;
        frame.ci += 1;
        if (next === frame.parent) continue;
        if (!disc.has(next)) {
          if (frame.node === start) rootChildren.add(next);
          disc.set(next, timer);
          low.set(next, timer);
          timer += 1;
          stack.push({ node: next, parent: frame.node, ci: 0 });
        } else {
          low.set(frame.node, Math.min(low.get(frame.node)!, disc.get(next)!));
        }
      } else {
        stack.pop();
        const parent = frame.parent;
        if (parent !== null) {
          low.set(parent, Math.min(low.get(parent)!, low.get(frame.node)!));
          if (parent !== start && low.get(frame.node)! >= disc.get(parent)!) {
            if (!result.includes(parent)) result.push(parent);
          }
        }
      }
    }
    if (rootChildren.size > 1) {
      if (!result.includes(start)) result.push(start);
    }
  }
  return result;
}

/**
 * Heuristic quorum-intersection check via connected components of the
 * undirected direct-trust projection.
 *
 * This is not a formal FBA proof, but it catches the common
 * "network split in two" failure mode: if two or more *significant*
 * components exist (components containing ≥ 3 nodes that publish their
 * own quorum set), the network has lost quorum intersection. Tiny
 * components of unpublished/long-tail nodes are ignored so isolated
 * observers do not trigger false alarms.
 *
 * Runs in O(N + E), so it stays cheap at 2,000-node scale.
 */
export function checkQuorumIntersection(
  nodes: readonly GraphNode[],
  direct: Map<string, Set<string>>,
): boolean {
  const adj = new Map<string, string[]>();
  for (const node of nodes) adj.set(node.id, []);
  for (const [src, targets] of direct) {
    const list = adj.get(src);
    if (list === undefined) continue;
    for (const t of targets) {
      list.push(t);
      adj.get(t)?.push(src);
    }
  }

  // Iterative connected-components labeling.
  const compOf = new Map<string, number>();
  const compPublished: number[] = [];
  let compCount = 0;
  for (const start of adj.keys()) {
    if (compOf.has(start)) continue;
    let published = 0;
    const stack = [start];
    compOf.set(start, compCount);
    while (stack.length > 0) {
      const cur = stack.pop()!;
      if (direct.has(cur)) published += 1;
      for (const next of adj.get(cur) ?? []) {
        if (!compOf.has(next)) {
          compOf.set(next, compCount);
          stack.push(next);
        }
      }
    }
    compPublished[compCount] = published;
    compCount += 1;
  }

  const significant = compPublished.filter((c) => c >= 3).length;
  return significant <= 1;
}
