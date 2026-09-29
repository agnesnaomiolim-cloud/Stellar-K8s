#!/usr/bin/env node
/**
 * simulate-partition.mjs
 *
 * Simulates a 50-node Stellar validator cluster and injects a network
 * partition event to validate that the WebGL topology dashboard:
 *
 *  1. Renders all 50 nodes as green (synced) during normal operation.
 *  2. Immediately severs spatial links for the isolated partition group.
 *  3. Highlights isolated nodes in red (partitioned).
 *  4. Recovers and restores green nodes when the partition heals.
 *
 * The script acts as a mock stellar-core WebSocket server, feeding
 * pre-crafted TopologyFrame JSON to any connected dashboard client.
 *
 * Usage
 * ──────
 *   node simulate-partition.mjs [--port 8765] [--partition-at 5000] [--heal-at 15000]
 *
 * Then open telemetry/dashboard/index.html in a browser and point it at
 *   ws://localhost:8765/ws/scp-topology
 *
 * Expected result
 * ────────────────
 *  0 s   – all 50 nodes green, full mesh of edges.
 *  5 s   – nodes 40-49 turn red, edges to partition group removed.
 *  15 s  – all 50 nodes turn green again, edges restored.
 *
 * Exit codes
 * ──────────
 *  0 – simulation completed (partition injected and healed).
 *  1 – usage / argument error.
 */

import { WebSocketServer } from 'ws';
import { createServer } from 'http';

// ─────────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────────

const args = parseArgs(process.argv.slice(2));
const PORT          = args['--port']         ?? 8765;
const PARTITION_AT  = args['--partition-at'] ?? 5_000;   // ms
const HEAL_AT       = args['--heal-at']      ?? 15_000;  // ms
const NODE_COUNT    = 50;
const TICK_MS       = 500; // topology frame interval

// ─────────────────────────────────────────────────────────────────────────────
// Node & edge generation
// ─────────────────────────────────────────────────────────────────────────────

/** Generate a mock Stellar public key for node i. */
function makeNodeId(i) {
  return `G${'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'.repeat(3).slice(i % 32, i % 32 + 7)}${String(i).padStart(4, '0')}`;
}

const NODE_IDS = Array.from({ length: NODE_COUNT }, (_, i) => makeNodeId(i));

/**
 * Build a topology frame.
 *
 * @param {number}   seq
 * @param {Set<string>} partitionedIds - node IDs that are isolated
 */
function buildFrame(seq, partitionedIds) {
  const now = Date.now();
  const ledger = 1_000_000 + seq;

  // Nodes
  const nodes = NODE_IDS.map((id, i) => {
    const isPartitioned = partitionedIds.has(id);
    return {
      id,
      label:       id.slice(0, 8),
      address:     `10.0.${Math.floor(i / 256)}.${i % 256}:11626`,
      phase:       isPartitioned ? 'OFFLINE'      : 'EXTERNALIZE',
      health:      isPartitioned ? 'partitioned'  : 'synced',
      peer_count:  isPartitioned ? 0              : 6,
      ledger_seq:  isPartitioned ? ledger - 100   : ledger,
      updated_at_ms: now,
    };
  });

  // Edges: ring topology + some cross links for visual richness.
  // Edges involving a partitioned node on either end are omitted.
  const edges = [];
  for (let i = 0; i < NODE_COUNT; i++) {
    const src = NODE_IDS[i];
    // Ring: each node connects to its two neighbours.
    for (const delta of [1, 2]) {
      const tgt = NODE_IDS[(i + delta) % NODE_COUNT];
      if (partitionedIds.has(src) || partitionedIds.has(tgt)) continue;
      edges.push({ source: src, target: tgt, direction: 'outbound', latency_ms: 10 + (i % 30) });
    }
    // Occasional long-distance link for visual clusters.
    if (i % 5 === 0) {
      const tgt = NODE_IDS[(i + 17) % NODE_COUNT];
      if (!partitionedIds.has(src) && !partitionedIds.has(tgt)) {
        edges.push({ source: src, target: tgt, direction: 'outbound', latency_ms: 80 });
      }
    }
  }

  return {
    seq,
    timestamp_ms: now,
    nodes,
    edges,
    local_ledger: ledger,
    partition_detected: partitionedIds.size > 0,
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// WebSocket server
// ─────────────────────────────────────────────────────────────────────────────

const httpServer = createServer((req, res) => {
  if (req.url === '/healthz') {
    res.writeHead(200);
    res.end('ok');
  } else {
    res.writeHead(404);
    res.end();
  }
});

const wss = new WebSocketServer({ server: httpServer, path: '/ws/scp-topology' });

const clients = new Set();

wss.on('connection', (ws, req) => {
  console.log(`[sim] Client connected from ${req.socket.remoteAddress}`);
  clients.add(ws);

  ws.on('close', () => {
    clients.delete(ws);
    console.log('[sim] Client disconnected');
  });

  ws.on('error', (e) => {
    console.error('[sim] WS error:', e.message);
    clients.delete(ws);
  });
});

function broadcast(frame) {
  const json = JSON.stringify(frame);
  for (const ws of clients) {
    if (ws.readyState === ws.OPEN) {
      ws.send(json);
    }
  }
}

// ─────────────────────────────────────────────────────────────────────────────
// Simulation loop
// ─────────────────────────────────────────────────────────────────────────────

let seq            = 0;
let partitionedIds = new Set();
let partitioned    = false;
let healed         = false;

const startTime = Date.now();

const ticker = setInterval(() => {
  const elapsed = Date.now() - startTime;

  // Inject partition at PARTITION_AT ms.
  if (!partitioned && elapsed >= PARTITION_AT) {
    partitioned    = true;
    // Isolate nodes 40-49 (last 10 nodes).
    partitionedIds = new Set(NODE_IDS.slice(40));
    console.log(`[sim] ⚡ PARTITION INJECTED at ${elapsed} ms – ${partitionedIds.size} nodes isolated`);
    console.log(`[sim]    Isolated nodes: ${[...partitionedIds].map(id => id.slice(0,8)).join(', ')}`);
  }

  // Heal partition at HEAL_AT ms.
  if (partitioned && !healed && elapsed >= HEAL_AT) {
    healed         = true;
    partitionedIds = new Set();
    console.log(`[sim] ✅ PARTITION HEALED at ${elapsed} ms – all nodes restored`);
  }

  const frame = buildFrame(seq++, partitionedIds);
  broadcast(frame);

  if (seq % 10 === 0) {
    const status = partitioned && !healed ? '⚡ PARTITIONED' : '✅ HEALTHY';
    console.log(`[sim] seq=${seq} clients=${clients.size} ledger=${frame.local_ledger} ${status}`);
  }

  // Stop after heal + 5 s.
  if (healed && elapsed >= HEAL_AT + 5000) {
    console.log('[sim] Simulation complete – shutting down');
    clearInterval(ticker);
    wss.close();
    httpServer.close();
    process.exit(0);
  }
}, TICK_MS);

httpServer.listen(PORT, () => {
  console.log(`[sim] Mock SCP topology server listening on ws://localhost:${PORT}/ws/scp-topology`);
  console.log(`[sim] Topology: ${NODE_COUNT} nodes`);
  console.log(`[sim] Partition at ${PARTITION_AT} ms, heal at ${HEAL_AT} ms`);
  console.log(`[sim] Open telemetry/dashboard/index.html and connect to ws://localhost:${PORT}/ws/scp-topology`);
});

// ─────────────────────────────────────────────────────────────────────────────
// Utilities
// ─────────────────────────────────────────────────────────────────────────────

function parseArgs(argv) {
  const result = {};
  for (let i = 0; i < argv.length; i++) {
    if (argv[i].startsWith('--') && i + 1 < argv.length) {
      const val = Number(argv[i + 1]);
      result[argv[i]] = Number.isNaN(val) ? argv[i + 1] : val;
      i++;
    }
  }
  return result;
}
