/// <reference lib="webworker" />
import { forceSimulation, forceLink, forceManyBody, forceCenter, forceX, forceY, forceZ } from 'd3-force-3d';
import type { SimulationNodeDatum } from 'd3-force-3d';
import type { LayoutRequest, LayoutResponse, LayoutTickPayload } from './layout.js'

interface SimNode extends SimulationNodeDatum {
  id: string;
  r: number;
}

/** Heavier nodes repel a bit more, spreading hubs apart. */
const CHARGE_PER_UNIT_RADIUS = -1.2;

let sim: ReturnType<typeof forceSimulation> | null = null;
let tickCount = 0;
let nodes: SimNode[] = [];
let nodeIndex = new Map<string, SimNode>();
let opts = { maxTicks: 3000, energyThreshold: 0.0025, maxTicksIdle: 300 };

function post(tickPayload: LayoutTickPayload): void {
  const msg: LayoutResponse = { type: 'layout:tick', payload: tickPayload };
  self.postMessage(msg);
}

self.onmessage = (event: MessageEvent<LayoutRequest>): void => {
  const msg = event.data;
  if (msg.type !== 'layout:init' && msg.type !== 'layout:update') return;

  const { nodes: inNodes, edges: inEdges } = msg.graph;
  const merged = { ...opts, ...msg.options };

  const nextIndex = new Map<string, SimNode>();
  for (const n of inNodes) {
    const prev = nodeIndex.get(n.id);
    const degree = n.trusters + n.trusting;
    nextIndex.set(n.id, {
      id: n.id,
      r: 0.6 + Math.sqrt(degree) * 0.35,
      x: prev?.x ?? (Math.random() - 0.5) * 600,
      y: prev?.y ?? (Math.random() - 0.5) * 600,
      z: prev?.z ?? (Math.random() - 0.5) * 600,
      vx: prev?.vx ?? 0,
      vy: prev?.vy ?? 0,
      vz: prev?.vz ?? 0,
    });
  }
  nodes = [...nextIndex.values()];
  nodeIndex = nextIndex;

  const links = inEdges.map((e) => ({
    source: e.source,
    target: e.target,
  }));

  sim?.stop();
  sim = forceSimulation(nodes, 3)
    .force(
      'link',
      forceLink(links, 3)
        .id((d) => (d as SimNode).id)
        .distance(merged.linkDistance ?? 30)
        .strength(0.4),
    )
    .force(
      'charge',
      forceManyBody()
        .strength(
          (d) => (d as SimNode).r * (merged.chargeStrength ?? -60) * CHARGE_PER_UNIT_RADIUS,
        )
        .distanceMax(merged.distanceMax ?? 420),
    )
    .force('center', forceCenter())
    .force('x', forceX(0).strength(0.03))
    .force('y', forceY(0).strength(0.03))
    .force('z', forceZ(0).strength(0.03))
    .velocityDecay(merged.velocityDecay ?? 0.42)
    .alphaMin(merged.energyThreshold)
    .on('tick', onTick);

  tickCount = 0;
};

function onTick(): void {
  tickCount += 1;

  // Kinetic-energy early exit: once the layout has settled, stop ticking so
  // the worker does not burn CPU and the main thread stops receiving
  // position floods that would cause jank.
  let energy = 0;
  for (const n of nodes) {
    energy += n.vx! * n.vx! + n.vy! * n.vy! + n.vz! * n.vz!;
  }
  const settled = energy < opts.energyThreshold * nodes.length;

  const tickPayload: LayoutTickPayload = {
    nodes: nodes.map((n) => ({ id: n.id, x: n.x!, y: n.y!, z: n.z! })),
    alpha: sim?.alpha() ?? 0,
    tick: tickCount,
    done: settled || tickCount >= opts.maxTicks,
  };
  post(tickPayload);

  if (tickPayload.done) {
    sim?.stop();
  }
}
