# Quorum Topology Visualizer (WebGL)

Interactive 3D visualization of Stellar's Federated Byzantine Agreement (FBA)
trust topology. Parses stellar-core quorum set JSON dumps into a directed trust
graph and renders up to **2,000 nodes at 60 FPS** using hardware-accelerated
WebGL (`three.js`) with force-directed physics (`d3-force-3d`) running in a
Web Worker.

Implements [#288 — WebGL Network Quorum Topology Visualizer].

## Features

| Feature | Details |
| --------------------- | -------------------------------------------------------------- |
| **Multi-shape parser** | Accepts `{node, known_peers}` core dumps, `/quorum` flat lists, bare arrays, and pubkey-keyed maps. Malformed input raises `QuorumParseError` with the JSON path. |
| **Trust classification** | Edges are colored by trust strength: **direct** (green), **indirect/transitive** (blue), **missing** (red — the peer publishes no quorum set of its own). |
| **SPOF detection** | Iterative Tarjan articulation-point analysis on the undirected trust projection flags nodes whose removal would fragment the graph. |
| **Quorum intersection** | O(N+E) connected-component heuristic warns when the network splits into multiple significant components. |
| **Realtime updates** | `QuorumWsClient` subscribes to `ws(s)://…/ws/quorum` with exponential backoff + jitter, feeding live quorum dumps into the graph without losing the layout. |
| **Inspection** | Click any node for a metadata panel: trust degrees, published-qset status, articulation-point badge, home domain, full public key. |
| **60 FPS HUD** | Rolling FPS histogram + frame-time readout, measured inside the render loop. |

## Quick start

```bash
cd frontend
npm install
npm run dev          # http://localhost:5173
```

The app loads the bundled mainnet-scale snapshot from
`public/snapshots/quorum_snapshot.json` (generated at stress scale:
~1,800 nodes). Switch to **Live (WebSocket)** in the toolbar to stream from a
running relay.

### Generating snapshots

```bash
# scale 1 ≈ today's mainnet magnitude (~500 nodes)
node scripts/generate_snapshot.mjs public/snapshots/quorum_snapshot.json 1

# scale 4 = 2,000-node stress target
node scripts/generate_snapshot.mjs public/snapshots/quorum_snapshot.json 4
```

The generator is seeded (deterministic output per scale) and models a
realistic topology: organizations running multiple validators with nested
inner quorum sets, independents, and long-tail validators that never publish
their own qset (the `missing` observability gaps).

### WebSocket message format

The visualizer expects a server pushing either:

```json
{ "type": "quorum", "dump": { "...": "quorum dump" } }
{ "type": "quorum:update", "node": {}, "known_peers": {} }
```

Any stellar-core quorum dump shape accepted by the parser works as the
payload.

## Performance architecture (why it holds 60 FPS)

1. **One draw call per geometry class** — all nodes render through a single
   `THREE.InstancedMesh` (one `SphereGeometry`, per-instance colors and
   matrices); all edges through one `LineSegments` with vertex colors.
2. **Physics off the main thread** — `layout.worker.ts` runs the
   `d3-force-3d` simulation in a Web Worker and posts position deltas per
   tick; the main thread only copies positions into instance matrices.
3. **Simulation settles** — the worker stops ticking once total kinetic
   energy drops below a threshold (or `maxTicks`), eliminating steady-state
   CPU burn and position floods.
4. **Render-on-demand** — the render loop skips `renderer.render` unless a
   tick, camera move, or selection marked the scene dirty. Idle GPU work is
   zero.
5. **Typed-array pipelines** — positions live in `Float32Array`s; edge
   endpoints are updated in place, no per-frame allocations.

Benchmarks live in `benchmarks/topology-visualizer/` — see
[RESULTS.md](../benchmarks/topology-visualizer/RESULTS.md) for the measured
frame budget and the methodology.

## Module map

```
frontend/
├── src/
│   ├── topology/
│   │   ├── webgl_renderer.ts    # three.js renderer: instancing, picking, camera
│   │   ├── layout.worker.ts     # d3-force-3d physics in a Web Worker
│   │   ├── layout.ts            # worker message contract + options
│   │   ├── graph.ts             # trust graph builder, Tarjan, intersection
│   │   ├── parser.ts            # stellar-core dump normalization
│   │   ├── ws_client.ts         # reconnecting WebSocket client
│   │   └── types.ts             # domain types
│   ├── components/
│   │   ├── force_graph.tsx      # renderer + worker bridge (no per-frame React)
│   │   ├── node_panel.tsx       # node metadata inspector
│   │   ├── hud.tsx              # network statistics
│   │   ├── fps_meter.tsx        # imperative FPS histogram
│   │   ├── toolbar.tsx          # source switcher, fit view, live status
│   │   └── legend.tsx           # color legend
│   └── App.tsx                  # data loading, snapshot/live ingestion
├── public/snapshots/            # generated quorum snapshots
├── scripts/generate_snapshot.mjs
└── tests/                       # node:test suites + headless benchmark
```

## Controls

| Input | Action |
| ------------- | ------------------------------ |
| Left-drag | Orbit |
| Right-drag / two-finger | Pan |
| Scroll / pinch | Zoom |
| Click node | Select + open metadata panel |
| `Fit view` | Frame the whole graph |

## Validation checklist (issue acceptance)

- [x] Parse stellar-core quorum set JSON dumps into a directed graph
      (`src/topology/parser.ts`, `src/topology/graph.ts`; unit-tested).
- [x] Three.js + WebGL rendering of nodes as 3D spheres, physical spacing via
      `d3-force-3d` (`src/topology/webgl_renderer.ts`, `layout.worker.ts`).
- [x] Color-coded edges for direct / indirect / missing trust.
- [x] Real-time updates via WebSocket (`ws_client.ts`, toolbar Live mode).
- [x] 2,000-node stress snapshot loads and renders smoothly
      (`scripts/generate_snapshot.mjs`, scale 4; included in `public/`).
- [x] Smooth zoom, pan, and node-click metadata inspection (OrbitControls +
      instanced raycast picking).
- [x] Performance benchmarks demonstrating 60 FPS headroom
      (`frontend/tests/bench/`, results in `benchmarks/topology-visualizer/`).
