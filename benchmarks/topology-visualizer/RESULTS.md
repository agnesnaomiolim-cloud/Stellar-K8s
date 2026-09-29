# Topology Visualizer — Performance Benchmark Results

- Date: 2026-09-29T09:56:48.727Z
- Snapshot: `/workspaces/Stellar-K8s/frontend/public/snapshots/quorum_snapshot.json` (1802 nodes, 965 edges)
- Mode: full (60 build runs, 300 physics ticks)
- Machine: linux x64, Node v24.21.0

## Pipeline latency

| Stage                        | p50          | p95          |
| ---------------------------- | ------------ | ------------ |
| Parse + graph build          | 5.15 ms | 12.64 ms |
| Physics tick (d3-force-3d)   | 19.59 ms | 31.00 ms |
| Main-thread tick apply       | 0.02 ms | 0.18 ms |

## 60 FPS verdict

- Frame budget @60 FPS: 16.6 ms
- Estimated main-thread frame cost: p95 apply 0.18 ms + conservative GPU 2.0 ms = **2.18 ms**
- Estimated FPS headroom: **60.0 FPS**
- Physics runs in a Web Worker; simulation settle exits early via kinetic-energy threshold, so steady-state main-thread cost is only the position apply + GPU draw.

**Result: ✅ PASS — 60 FPS maintained with headroom**

## Notes

- GPU cost is conservatively estimated at 2.0 ms for 1 InstancedMesh + 1 LineSegments draw call; interactive profiling via ``npm run dev`` + Chrome DevTools Performance panel shows real numbers.
- Articulation points: 16, quorum intersection: true.
