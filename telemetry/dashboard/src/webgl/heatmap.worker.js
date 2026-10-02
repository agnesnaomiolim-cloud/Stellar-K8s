/**
 * @file heatmap.worker.js
 * @description
 * Web Worker that owns the WebSocket connection to the Rust telemetry backend
 * and forwards normalised {@link PodTrafficSnapshot} messages to the main
 * thread where the WebGL renderer lives.
 *
 * ## Architecture
 *
 * ```
 *  Main thread                      Worker thread
 *  ┌────────────────────┐           ┌──────────────────────────────────┐
 *  │ TrafficHeatmap      │ <─────── │ HeatmapWorker                    │
 *  │ (WebGL rendering)   │ postMsg  │  ├─ WebSocket → Rust WS endpoint │
 *  │                     │          │  ├─ Reconnect / backoff logic     │
 *  │ new Worker(...)     │ ───────> │  ├─ Snapshot validation          │
 *  │ worker.postMessage( │ connect  │  └─ Rate-limiter (max 60fps)     │
 *  │  { type:'connect',  │          └──────────────────────────────────┘
 *  │    url: wsUrl })    │
 *  └────────────────────┘
 * ```
 *
 * ## Messages: main → worker
 *
 * | `type`        | payload fields | description |
 * |---|---|---|
 * | `connect`     | `url` (string) | Open WebSocket to the given URL |
 * | `disconnect`  | —              | Close the WebSocket cleanly |
 * | `ping`        | —              | Echo back a `pong` |
 *
 * ## Messages: worker → main
 *
 * | `type`        | payload fields | description |
 * |---|---|---|
 * | `snapshot`    | `data` (PodTrafficSnapshot) | Parsed snapshot ready to render |
 * | `bulk`        | `snapshots` (array)         | Initial hydration batch |
 * | `status`      | `connected` (bool), `url`   | Connection state change |
 * | `error`       | `message` (string)          | Non-fatal error for the UI |
 * | `pong`        | —                           | Response to `ping` |
 *
 * @module webgl/heatmap.worker
 */

'use strict';

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Maximum number of snapshot updates forwarded to the main thread per second.
 *  Keeps postMessage overhead below 1% of a 60fps frame budget. */
const MAX_FPS = 60;
const FRAME_BUDGET_MS = 1000 / MAX_FPS;

/** WebSocket reconnect back-off parameters (milliseconds). */
const RECONNECT_BASE_MS  = 500;
const RECONNECT_MAX_MS   = 30_000;
const RECONNECT_FACTOR   = 1.5;

// ---------------------------------------------------------------------------
// Worker state
// ---------------------------------------------------------------------------

/** @type {WebSocket|null} */
let socket        = null;
let socketUrl     = null;
let reconnectMs   = RECONNECT_BASE_MS;
let reconnectTimer = null;
let intentionalClose = false;

/** Pending snapshot updates accumulated since the last flush. */
const pendingUpdates = new Map(); // pod_name → latest snapshot
let lastFlushTs = 0;
let flushTimer  = null;

// ---------------------------------------------------------------------------
// Message handler (main thread → worker)
// ---------------------------------------------------------------------------

self.addEventListener('message', (event) => {
    const { type, ...payload } = event.data || {};

    switch (type) {
        case 'connect':
            if (!payload.url) {
                postError('connect message missing "url" field');
                return;
            }
            intentionalClose = false;
            openSocket(payload.url);
            break;

        case 'disconnect':
            intentionalClose = true;
            closeSocket();
            break;

        case 'ping':
            self.postMessage({ type: 'pong' });
            break;

        default:
            postError(`Unknown message type: "${type}"`);
    }
});

// ---------------------------------------------------------------------------
// WebSocket lifecycle
// ---------------------------------------------------------------------------

/**
 * Open (or re-open) a WebSocket connection to the given URL.
 *
 * @param {string} url - e.g. `ws://localhost:9200/ws/envoy-stats`
 */
function openSocket(url) {
    if (socket && socket.readyState <= WebSocket.OPEN) {
        socket.close(1000, 'replaced');
    }

    socketUrl = url;
    socket    = new WebSocket(url);

    socket.addEventListener('open', onOpen);
    socket.addEventListener('message', onMessage);
    socket.addEventListener('close', onClose);
    socket.addEventListener('error', onSocketError);
}

function closeSocket() {
    clearTimeout(reconnectTimer);
    if (socket) {
        socket.removeEventListener('close', onClose); // prevent reconnect loop
        socket.close(1000, 'intentional close');
        socket = null;
    }
    self.postMessage({ type: 'status', connected: false, url: socketUrl });
}

// ---------------------------------------------------------------------------
// WebSocket event handlers
// ---------------------------------------------------------------------------

function onOpen() {
    reconnectMs = RECONNECT_BASE_MS; // reset back-off on successful connect
    self.postMessage({ type: 'status', connected: true, url: socketUrl });

    // Request an initial bulk hydration snapshot from the server.
    // The Rust handler responds with a JSON array under key "snapshots".
    try {
        socket.send(JSON.stringify({ cmd: 'hydrate' }));
    } catch (_) {
        // Server may not support the hydrate command; ignore.
    }
}

function onMessage(event) {
    let parsed;

    try {
        parsed = JSON.parse(event.data);
    } catch (e) {
        postError(`Failed to parse WebSocket message: ${e.message}`);
        return;
    }

    // ── Bulk hydration response ──────────────────────────────────────────
    if (Array.isArray(parsed.snapshots)) {
        const valid = parsed.snapshots.filter(validateSnapshot);
        if (valid.length > 0) {
            self.postMessage({ type: 'bulk', snapshots: valid });
        }
        return;
    }

    // ── Single snapshot update ───────────────────────────────────────────
    if (validateSnapshot(parsed)) {
        queueUpdate(parsed);
        return;
    }

    // ── Server-side error frames ─────────────────────────────────────────
    if (parsed.error) {
        postError(`Server error: ${parsed.error}`);
    }
}

function onClose(event) {
    self.postMessage({ type: 'status', connected: false, url: socketUrl });

    if (intentionalClose) return;

    // Schedule reconnect with exponential back-off.
    reconnectTimer = setTimeout(() => {
        openSocket(socketUrl);
    }, reconnectMs);

    reconnectMs = Math.min(reconnectMs * RECONNECT_FACTOR, RECONNECT_MAX_MS);
}

function onSocketError(event) {
    // Errors are always followed by a close event, so the reconnect logic in
    // onClose handles recovery.  We just surface the fact to the UI.
    postError('WebSocket connection error — will attempt to reconnect');
}

// ---------------------------------------------------------------------------
// Rate-limited flush to the main thread
// ---------------------------------------------------------------------------

/**
 * Queue a snapshot update.  Multiple updates for the same pod within one
 * frame budget are coalesced — only the latest is forwarded.
 *
 * @param {object} snapshot
 */
function queueUpdate(snapshot) {
    pendingUpdates.set(snapshot.pod_name, snapshot);

    const now  = performance.now();
    const wait = FRAME_BUDGET_MS - (now - lastFlushTs);

    if (wait <= 0) {
        flushUpdates();
    } else if (!flushTimer) {
        flushTimer = setTimeout(flushUpdates, wait);
    }
}

function flushUpdates() {
    flushTimer  = null;
    lastFlushTs = performance.now();

    if (pendingUpdates.size === 0) return;

    for (const snapshot of pendingUpdates.values()) {
        self.postMessage({ type: 'snapshot', data: snapshot });
    }
    pendingUpdates.clear();
}

// ---------------------------------------------------------------------------
// Snapshot validation
// ---------------------------------------------------------------------------

/**
 * Validate that a parsed object looks like a PodTrafficSnapshot.
 *
 * Guards against schema drift between the Rust backend and the JS frontend.
 *
 * @param {unknown} obj
 * @returns {boolean}
 */
function validateSnapshot(obj) {
    if (!obj || typeof obj !== 'object') return false;
    if (typeof obj.pod_name    !== 'string') return false;
    if (typeof obj.heat_level  !== 'number') return false;
    if (obj.heat_level < 0 || obj.heat_level > 1) return false;

    // Coerce optional numeric fields to safe defaults.
    obj.active_requests   = typeof obj.active_requests   === 'number' ? obj.active_requests   : 0;
    obj.active_connections = typeof obj.active_connections === 'number' ? obj.active_connections : 0;
    obj.region            = typeof obj.region             === 'string'  ? obj.region             : null;

    return true;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Forward a non-fatal error message to the main thread UI.
 *
 * @param {string} message
 */
function postError(message) {
    self.postMessage({ type: 'error', message });
}
