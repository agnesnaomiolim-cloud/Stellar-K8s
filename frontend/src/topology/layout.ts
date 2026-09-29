import type { QuorumGraph, TrustKind } from './types.js';

/** Worker-side simulation tunables (mirrors the defaults used in the worker). */
export interface LayoutOptions {
  /** Base charge strength; more negative = stronger repulsion. */
  chargeStrength: number;
  /** Target edge length. */
  linkDistance: number;
  /** Velocity decay per tick. */
  velocityDecay: number;
  /** Distance cutoff of the many-body force. */
  distanceMax: number;
  /** Stop simulating after this many ticks even if not cooled. */
  maxTicks: number;
  /** Settled when total kinetic energy per node drops below this. */
  energyThreshold: number;
  /** Whether to reheat (reuse positions) on graph topology updates. */
  reheatOnUpdate: boolean;
}

export const DEFAULT_LAYOUT_OPTIONS: LayoutOptions = {
  chargeStrength: -60,
  linkDistance: 30,
  velocityDecay: 0.42,
  distanceMax: 420,
  maxTicks: 3000,
  energyThreshold: 0.0025,
  reheatOnUpdate: true,
};

export interface LayoutTickPayload {
  nodes: Array<{ id: string; x: number; y: number; z: number }>;
  alpha: number;
  tick: number;
  done: boolean;
}

export interface LayoutRequest {
  type: 'layout:init' | 'layout:update';
  graph: {
    nodes: Array<{ id: string; trusters: number; trusting: number }>;
    edges: Array<{ source: string; target: string; kind: TrustKind }>;
  };
  options?: Partial<LayoutOptions>;
}

export interface LayoutResponse {
  type: 'layout:tick';
  payload: LayoutTickPayload;
}

export interface GraphSummary {
  nodeCount: number;
  edgeCount: number;
  directCount: number;
  indirectCount: number;
  missingCount: number;
  articulationPoints: readonly string[];
  hasQuorumIntersection: boolean;
  publishedCount: number;
}

export function summarizeGraph(graph: QuorumGraph): GraphSummary {
  return {
    nodeCount: graph.stats.nodeCount,
    edgeCount: graph.stats.edgeCount,
    directCount: graph.stats.directCount,
    indirectCount: graph.stats.indirectCount,
    missingCount: graph.stats.missingCount,
    articulationPoints: graph.stats.articulationPoints,
    hasQuorumIntersection: graph.stats.hasQuorumIntersection,
    publishedCount: graph.stats.publishedCount,
  };
}
