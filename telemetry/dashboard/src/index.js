/**
 * @file index.js
 * @description
 * Dashboard entry point for the Dynamic WebGL Traffic Routing Heatmap (#329).
 *
 * Bootstraps the {@link TrafficHeatmap} WebGL renderer on the canvas element
 * and connects it to the {@link HeatmapWorker} data-ingestion thread.
 *
 * ## Embedding
 *
 * Add a canvas and a container to your HTML:
 *
 * ```html
 * <div id="heatmap-container">
 *   <canvas id="heatmap-canvas"></canvas>
 * </div>
 * <script type="module" src="./src/index.js"></script>
 * ```
 *
 * The WS endpoint URL is read from the `data-ws-url` attribute on the canvas
 * element, or from the global `window.HEATMAP_WS_URL`, or defaults to
 * `ws://localhost:9200/ws/envoy-stats`.
 */

'use strict';

import { TrafficHeatmap, heatToHex } from './webgl/heatmap.js';

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

/** @type {TrafficHeatmap|null} */
let heatmap = null;

/** @type {Worker|null} */
let worker  = null;

/**
 * Initialise the heatmap on the given canvas.
 *
 * @param {HTMLCanvasElement} canvas
 * @param {string}           wsUrl
 */
function init(canvas, wsUrl) {
    // --- WebGL renderer ---------------------------------------------------
    try {
        heatmap = new TrafficHeatmap(canvas, { targetFps: 60 });
        heatmap.start();
    } catch (err) {
        showError(`WebGL initialisation failed: ${err.message}`);
        return;
    }

    // --- Web Worker -------------------------------------------------------
    try {
        worker = new Worker(
            new URL('./webgl/heatmap.worker.js', import.meta.url),
            { type: 'module' },
        );
    } catch (err) {
        showError(`Failed to start heatmap worker: ${err.message}`);
        return;
    }

    worker.addEventListener('message', handleWorkerMessage);
    worker.addEventListener('error',   (e) => showError(`Worker error: ${e.message}`));

    // Connect the worker to the telemetry WebSocket.
    worker.postMessage({ type: 'connect', url: wsUrl });

    // --- Legend -----------------------------------------------------------
    renderLegend(canvas.parentElement);

    // --- Status badge -----------------------------------------------------
    updateStatusBadge(canvas.parentElement, false);
}

// ---------------------------------------------------------------------------
// Worker message handler
// ---------------------------------------------------------------------------

/**
 * Route messages from the Web Worker to the WebGL renderer.
 *
 * @param {MessageEvent} event
 */
function handleWorkerMessage(event) {
    const { type, data, snapshots, connected, message } = event.data;

    switch (type) {
        case 'snapshot':
            // Single real-time update.
            if (heatmap && data) {
                heatmap.updatePod(data);
            }
            break;

        case 'bulk':
            // Initial hydration batch from server.
            if (heatmap && Array.isArray(snapshots)) {
                heatmap.replaceAll(snapshots);
            }
            break;

        case 'status':
            updateStatusBadge(
                document.getElementById('heatmap-canvas')?.parentElement,
                connected,
            );
            if (!connected) {
                console.warn('[heatmap] Disconnected from telemetry stream');
            } else {
                console.info('[heatmap] Connected to telemetry stream');
            }
            break;

        case 'error':
            console.warn(`[heatmap worker] ${message}`);
            break;

        case 'pong':
            // Heartbeat response — no action needed.
            break;

        default:
            console.warn(`[heatmap] Unknown worker message type: "${type}"`);
    }
}

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

/**
 * Inject a colour-scale legend below the canvas.
 *
 * @param {HTMLElement|null} container
 */
function renderLegend(container) {
    if (!container) return;

    const existing = container.querySelector('.heatmap-legend');
    if (existing) existing.remove();

    const legend = document.createElement('div');
    legend.className = 'heatmap-legend';
    legend.style.cssText = [
        'display:flex',
        'align-items:center',
        'gap:4px',
        'margin-top:8px',
        'font:11px/1.4 monospace',
        'color:#94a3b8',
    ].join(';');

    const steps  = 40;
    const bar    = document.createElement('canvas');
    bar.width    = steps * 4;
    bar.height   = 12;
    bar.style.borderRadius = '3px';
    const ctx    = bar.getContext('2d');

    for (let i = 0; i < steps; i++) {
        ctx.fillStyle = heatToHex(i / (steps - 1));
        ctx.fillRect(i * 4, 0, 4, 12);
    }

    const labelCold = document.createElement('span');
    labelCold.textContent = 'Idle';

    const labelHot = document.createElement('span');
    labelHot.textContent  = 'Overloaded';

    legend.appendChild(labelCold);
    legend.appendChild(bar);
    legend.appendChild(labelHot);

    container.appendChild(legend);
}

/**
 * Show / hide a small connection-status badge.
 *
 * @param {HTMLElement|null} container
 * @param {boolean}          connected
 */
function updateStatusBadge(container, connected) {
    if (!container) return;

    let badge = container.querySelector('.heatmap-status');
    if (!badge) {
        badge = document.createElement('span');
        badge.className  = 'heatmap-status';
        badge.style.cssText = [
            'position:absolute',
            'top:8px',
            'right:8px',
            'padding:2px 8px',
            'border-radius:12px',
            'font:10px/1.6 monospace',
            'pointer-events:none',
        ].join(';');
        container.style.position = 'relative';
        container.appendChild(badge);
    }

    if (connected) {
        badge.textContent   = '● LIVE';
        badge.style.background = 'rgba(34,197,94,0.2)';
        badge.style.color      = '#22c55e';
        badge.style.border     = '1px solid rgba(34,197,94,0.4)';
    } else {
        badge.textContent   = '○ RECONNECTING…';
        badge.style.background = 'rgba(234,179,8,0.2)';
        badge.style.color      = '#eab308';
        badge.style.border     = '1px solid rgba(234,179,8,0.4)';
    }
}

/**
 * Display a full-canvas error overlay.
 *
 * @param {string} message
 */
function showError(message) {
    const canvas = document.getElementById('heatmap-canvas');
    if (!canvas) {
        console.error(`[heatmap] ${message}`);
        return;
    }

    const overlay = document.createElement('div');
    overlay.style.cssText = [
        'position:absolute',
        'inset:0',
        'display:flex',
        'align-items:center',
        'justify-content:center',
        'background:rgba(0,0,0,0.7)',
        'color:#f87171',
        'font:13px/1.5 monospace',
        'padding:16px',
        'text-align:center',
        'z-index:100',
    ].join(';');
    overlay.textContent = `⚠ Heatmap error: ${message}`;

    canvas.parentElement.style.position = 'relative';
    canvas.parentElement.appendChild(overlay);
}

// ---------------------------------------------------------------------------
// Auto-initialise on DOMContentLoaded
// ---------------------------------------------------------------------------

function autoInit() {
    const canvas = document.getElementById('heatmap-canvas');
    if (!canvas) {
        console.warn('[heatmap] No element with id="heatmap-canvas" found');
        return;
    }

    const wsUrl =
        canvas.dataset.wsUrl ||
        window.HEATMAP_WS_URL ||
        'ws://localhost:9200/ws/envoy-stats';

    init(canvas, wsUrl);
}

if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', autoInit);
} else {
    autoInit();
}

// ---------------------------------------------------------------------------
// Exports (for programmatic embedding)
// ---------------------------------------------------------------------------

export { init, heatmap, worker };
