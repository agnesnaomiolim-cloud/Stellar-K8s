# Stellar Network Topology Visualizer

A standalone React + Three.js SPA for inspecting multi-cluster SCP quorum topology. The graph uses instanced WebGL spheres for validators and one batched line buffer for quorum links. This keeps the scene object count constant while the graph updates.

## Run It

From `frontend/analytics`:

```bash
npm install
npm run dev
```

Open the Vite URL printed by the command. The default source is the operator WebSocket endpoint:

```text
/api/v1/quorum/topology/stream
```

The operator stream sends `QuorumTopologyResponse` snapshots every five seconds. The frontend also accepts individual JSON `ScpMessage` records. For a real Kafka topic, run `npm run stream:kafka`; the KafkaJS bridge consumes `KAFKA_TOPIC` from `KAFKA_BROKERS` and broadcasts JSON messages to browser clients over WebSocket.

## Mock Load Test

Start the deterministic 500-node / 2,000-edge stream in a second terminal:

```bash
npm run mock:stream
```

Choose **Mock Kafka stream** in the app, or open the app with `?source=mock`. Customize the workload when measuring hardware:

```bash
node scripts/mock-kafka-stream.mjs --serve --nodes 500 --edges 2000 --interval 120
```

For a real topic bridge:

```bash
KAFKA_BROKERS=localhost:9092 KAFKA_TOPIC=stellar-scp-messages npm run stream:kafka
```

Set `KAFKA_FROM_BEGINNING=true` to replay retained messages. The bridge expects JSON values, matching the existing JSON serialization path in the operator pipeline.

The generator sends one initial snapshot and then individual SCP messages containing phase, ballot, TPS, ledger time, and quorum-set updates. Without `--serve`, it writes newline-delimited JSON records to stdout for replay or piping into a Kafka producer.

## Data Mapping

Snapshot nodes use the existing operator fields: `id`, `full_id`, `phase`, `is_critical`, `threshold`, and `stalled`. Individual messages use the fields in `schemas/scp_message.proto`. TPS and ledger time are read from `metrics.tps`, `metrics.ledger_time_ms`, or equivalent snake/camel-case fields and metadata. The current repository SCP schemas do not define those two measurements, so live producers must enrich the message or metadata for the inspector to show them; the mock stream includes both.

Node colors indicate health: green is synced, amber is degraded, and red is falling behind or unknown. Click a node to inspect cluster, SCP phase, ballot, TPS, ledger time, and quorum threshold. OrbitControls provides drag-to-orbit, scroll-to-zoom, and pan interaction.

## Checks

```bash
npm test
npm run build
npm run matrix:perf
```

The model tests exercise both snapshot and message ingestion. The performance harnesses validate the issue's 10,000-cell requirement and produce the profiling evidence in [PROFILING.md](./PROFILING.md):

```bash
npm run build && npm run matrix:browser:perf            # fps, long tasks, heap in headless Chromium
npm run build && npm run matrix:browser:perf -- --video # + interactive navigation screencast
```

`matrix:perf` fails if the mock topology yields fewer than 10,000 interconnect cells. The browser harness records the actual WebGL renderer string; numbers captured on GPU-less machines (SwiftShader rasterization) are environment-bound and are labeled as such. The renderer avoids per-edge/per-node React elements and limits device pixel ratio to reduce GPU pressure.
