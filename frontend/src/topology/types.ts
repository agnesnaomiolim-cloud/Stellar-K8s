/** Domain types for the quorum trust topology. */

/** Classification of an edge in the trust graph. */
export type TrustKind = 'direct' | 'indirect' | 'missing';

/** Reason a node or edge ended up in the `missing` category. */
export type MissingReason = 'not_in_dump' | 'transitive_only';

export interface GraphNode {
  /** Stellar node public key (`G…`). */
  readonly id: string;
  /** Short human label (first/last 4 chars of the key). */
  readonly label: string;
  /** Home domain advertised in the quorum set, if any. */
  readonly domain?: string;
  /** True when this node published its own quorum set in the dump. */
  readonly hasQuorumSet: boolean;
  /** Number of nodes that list this node in their quorum set (in-degree). */
  readonly trusters: number;
  /** Number of nodes this node lists in its quorum set (out-degree). */
  readonly trusting: number;
  /** True if removing this node disconnects part of the trust graph. */
  readonly isArticulationPoint: boolean;
}

export interface GraphEdge {
  /** Source node id (the trusting node). */
  readonly source: string;
  /** Target node id (the trusted node). */
  readonly target: string;
  /** How the trust should be colored. */
  readonly kind: TrustKind;
  /** Present when kind === 'missing'. */
  readonly reason?: MissingReason;
}

export interface QuorumGraph {
  readonly nodes: readonly GraphNode[];
  readonly edges: readonly GraphEdge[];
  readonly stats: GraphStats;
}

export interface GraphStats {
  readonly nodeCount: number;
  readonly edgeCount: number;
  readonly directCount: number;
  readonly indirectCount: number;
  readonly missingCount: number;
  readonly publishedCount: number;
  readonly articulationPoints: readonly string[];
  readonly hasQuorumIntersection: boolean;
}

/** A single node entry as it appears in a stellar-core quorum dump. */
export interface QuorumSetEntry {
  /** Node public key (`G…`). */
  id: string;
  /** Optional home domain, e.g. `sdf.example.org`. */
  name?: string;
  /** Full quorum set information, when the dump includes it. */
  qset?: QuorumSetLike | null;
}

/**
 * Shape of `VALIDATOR_QUORUM_SET` / `/quorum` info from stellar-core.
 * Nested sets use the recursive `quorumSets` field; some builds expose
 * `innerSets` instead — both are accepted.
 */
export interface QuorumSetLike {
  /** Fraction (e.g. 0.7) or count (e.g. 3) required for agreement. */
  threshold?: number;
  /** Flat validator member list. */
  validators?: readonly (string | QuorumSetEntry)[];
  /** Nested quorum sets. */
  quorumSets?: readonly QuorumSetLike[];
  /** Alternative field name for nested sets (older core builds). */
  innerSets?: readonly QuorumSetLike[];
}

export interface QuorumDump {
  /** Top-level quorum information for the observing node. */
  node?: QuorumSetEntry & { qset?: QuorumSetLike | null };
  /** Known peers keyed by public key, as in `KNOWN_PEERS`. */
  knownPeers?: Record<string, QuorumSetEntry>;
  /** Alternative: flat list of all entries (`/quorum` JSON endpoint). */
  nodes?: readonly QuorumSetEntry[];
}

/** Runtime statistics tracked by the FPS meter. */
export interface FpsStats {
  readonly fps: number;
  readonly frameMs: number;
  readonly samples: number;
}

export interface BenchmarkResult {
  readonly nodeCount: number;
  readonly edgeCount: number;
  readonly minFps: number;
  readonly avgFps: number;
  readonly p95FrameMs: number;
  readonly durationMs: number;
  readonly passed: boolean;
}
