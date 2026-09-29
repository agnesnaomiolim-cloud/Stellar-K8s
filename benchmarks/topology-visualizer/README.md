# Topology Visualizer Benchmarks

Performance evidence for the 60 FPS requirement of
[#288 — WebGL Network Quorum Topology Visualizer].

## Methodology

The pipeline is benchmarked headlessly in three stages that map 1:1 to the
production architecture:

1. **Parse + graph build** — runs once per load (not per frame). Measures
   stellar-core dump parsing, edge classification, Tarjan articulation
   points, and the quorum-intersection check.
2. **Physics tick** — `d3-force-3d` simulation step at 2,000 nodes /
   ~1,000–6,000 edges, identical to the code running in the Web Worker.
3. **Main-thread tick apply** — copying a worker position payload into
   typed arrays, the only per-frame main-thread work besides the GPU draw.

GPU rasterization cost cannot be measured headlessly, so the verdict uses a
**conservative 2.0 ms GPU estimate** for the actual scene (one InstancedMesh
draw call for all spheres + one LineSegments draw call for all edges) added
to the p95 apply cost. A result is a pass when the estimated steady-state
frame cost stays inside the 16.6 ms (60 FPS) budget with headroom
(≥ 55 FPS equivalent).

## Reproducing

```bash
cd frontend
npm run bench          # full: 60 build runs, 300 physics ticks
npm run bench:fast     # quick mode for CI
npm test               # includes fps_budget.test.mjs gate
```

Results are written to [RESULTS.md](RESULTS.md) on every run.

## Interactive verification

For GPU-truth numbers on your hardware:

1. `cd frontend && npm run dev`
2. Open Chrome DevTools → Performance panel → record while dragging the
   camera and letting the simulation settle.
3. Expected: frame times below 16.6 ms once the layout cools; the FPS HUD
   (bottom center) shows the live rolling histogram.
