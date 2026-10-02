/**
 * @file heatmap.js
 * @module webgl/heatmap
 * @description
 * WebGL-powered traffic-routing heatmap for the Stellar-K8s dashboard.
 *
 * Renders a dynamic topological map of Soroban RPC pods using custom GLSL
 * shaders.  Each pod is represented as a pulsating particle node whose colour
 * temperature maps to its current traffic load:
 *
 *   heat_level 0.0  →  blue   (#0033ff) — idle
 *   heat_level 0.5  →  yellow (#ffcc00) — moderate
 *   heat_level 1.0  →  white/red (#ff1100) — overloaded
 *
 * Rendering is entirely decoupled from data ingestion; the heatmap only
 * calls `updatePod()` / `removePod()` which can be invoked from a
 * {@link HeatmapWorker} message handler without blocking the GL loop.
 *
 * ## Usage
 *
 * ```js
 * import { TrafficHeatmap } from './heatmap.js';
 *
 * const heatmap = new TrafficHeatmap(document.getElementById('heatmap-canvas'));
 * heatmap.start();
 *
 * // Called from the Web Worker message handler (see heatmap.worker.js)
 * heatmap.updatePod({
 *   pod_name: 'soroban-rpc-abc',
 *   heat_level: 0.87,
 *   active_requests: 435,
 *   active_connections: 120,
 *   region: 'us-east-1',
 * });
 * ```
 *
 * @requires WebGL2
 */

'use strict';

// ---------------------------------------------------------------------------
// GLSL shaders
// ---------------------------------------------------------------------------

/**
 * Vertex shader.
 *
 * Each pod is a single point-sprite whose size pulses over time based on
 * `a_heat`.  The uniform `u_time` drives the animation.
 */
const VERTEX_SHADER_SRC = /* glsl */`#version 300 es
precision highp float;

// Per-pod attributes uploaded via the instance buffer.
in vec2  a_position;   // Normalised device coordinates [-1, 1]
in float a_heat;       // Normalised heat level [0, 1]
in float a_phase;      // Per-pod animation phase offset [0, 2π]

// Uniforms
uniform float u_time;        // Elapsed seconds
uniform vec2  u_resolution;  // Viewport dimensions in pixels

// Outputs to fragment shader
out float v_heat;
out float v_pulse;

void main() {
    // Pulsating point size: base 8px, up to +24px at full heat, with a
    // sinusoidal throb driven by u_time and per-pod phase.
    float pulse = 0.5 + 0.5 * sin(u_time * 3.0 + a_phase);
    float size  = 8.0 + a_heat * 24.0 + pulse * a_heat * 8.0;

    gl_Position  = vec4(a_position, 0.0, 1.0);
    gl_PointSize = size;

    v_heat  = a_heat;
    v_pulse = pulse;
}
`;

/**
 * Fragment shader.
 *
 * Renders each point-sprite as a soft circular disc.  Colour is interpolated
 * through a three-stop gradient: blue → yellow → white/red.
 */
const FRAGMENT_SHADER_SRC = /* glsl */`#version 300 es
precision highp float;

in float v_heat;
in float v_pulse;

out vec4 fragColor;

// Three-stop heatmap palette.
const vec3 COLD   = vec3(0.0,  0.2,  1.0);   // blue  – idle
const vec3 WARM   = vec3(1.0,  0.8,  0.0);   // yellow – moderate
const vec3 HOT    = vec3(1.0,  0.067, 0.0);  // red   – overloaded
const vec3 SATURE = vec3(1.0,  1.0,  0.95);  // near-white – saturated

vec3 heatColour(float t) {
    if (t < 0.5) {
        return mix(COLD, WARM, t * 2.0);
    } else if (t < 0.85) {
        return mix(WARM, HOT, (t - 0.5) * (1.0 / 0.35));
    } else {
        return mix(HOT, SATURE, (t - 0.85) * (1.0 / 0.15));
    }
}

void main() {
    // Soft disc: fade alpha toward the edge of the point-sprite quad.
    vec2  coord = gl_PointCoord * 2.0 - 1.0;
    float dist  = dot(coord, coord);          // squared distance from centre
    if (dist > 1.0) discard;                 // outside circle
    float alpha = 1.0 - smoothstep(0.6, 1.0, dist);

    // Bloom ring: bright halo at high heat.
    float halo  = smoothstep(0.5, 0.75, dist) * v_heat * v_pulse;
    vec3  colour = heatColour(v_heat) + halo * 0.4;

    fragColor = vec4(colour, alpha);
}
`;

// ---------------------------------------------------------------------------
// Edge / connection shader (lines between pods)
// ---------------------------------------------------------------------------

const EDGE_VERT_SRC = /* glsl */`#version 300 es
precision highp float;

in vec2  a_position;
in float a_flow;    // Normalised traffic flow on this edge [0, 1]

out float v_flow;

void main() {
    gl_Position = vec4(a_position, 0.0, 1.0);
    v_flow = a_flow;
}
`;

const EDGE_FRAG_SRC = /* glsl */`#version 300 es
precision highp float;

in float v_flow;
out vec4 fragColor;

void main() {
    // Dim teal line; opacity scales with traffic flow.
    vec3 edgeColour = vec3(0.1, 0.8, 0.7);
    fragColor = vec4(edgeColour, 0.15 + v_flow * 0.5);
}
`;

// ---------------------------------------------------------------------------
// Shader utilities
// ---------------------------------------------------------------------------

/**
 * Compile a GLSL shader.
 *
 * @param {WebGL2RenderingContext} gl
 * @param {number} type - `gl.VERTEX_SHADER` or `gl.FRAGMENT_SHADER`
 * @param {string} source
 * @returns {WebGLShader}
 */
function compileShader(gl, type, source) {
    const shader = gl.createShader(type);
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
        const info = gl.getShaderInfoLog(shader);
        gl.deleteShader(shader);
        throw new Error(`Shader compile error: ${info}`);
    }
    return shader;
}

/**
 * Link a vertex + fragment shader pair into a program.
 *
 * @param {WebGL2RenderingContext} gl
 * @param {string} vertSrc
 * @param {string} fragSrc
 * @returns {WebGLProgram}
 */
function buildProgram(gl, vertSrc, fragSrc) {
    const vert    = compileShader(gl, gl.VERTEX_SHADER,   vertSrc);
    const frag    = compileShader(gl, gl.FRAGMENT_SHADER, fragSrc);
    const program = gl.createProgram();
    gl.attachShader(program, vert);
    gl.attachShader(program, frag);
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
        const info = gl.getProgramInfoLog(program);
        gl.deleteProgram(program);
        throw new Error(`Program link error: ${info}`);
    }
    gl.deleteShader(vert);
    gl.deleteShader(frag);
    return program;
}

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

/**
 * Arrange pods in a pseudo-topological layout.
 *
 * Pods in the same region are clustered; regions are distributed in a ring
 * so the heatmap naturally groups co-located nodes.
 *
 * @param {Map<string, PodState>} pods
 * @returns {void}  Mutates `pod.ndcX / pod.ndcY` in-place.
 */
function computeLayout(pods) {
    /** @type {Map<string, string[]>} region → [pod_name] */
    const regions = new Map();
    for (const [name, pod] of pods) {
        const r = pod.region || 'default';
        if (!regions.has(r)) regions.set(r, []);
        regions.get(r).push(name);
    }

    const regionNames = Array.from(regions.keys());
    const regionCount = regionNames.length;

    regionNames.forEach((region, rIdx) => {
        // Region centroid on a unit circle, scaled to fit NDC with margin.
        const regionAngle = (rIdx / regionCount) * 2 * Math.PI;
        const rx = Math.cos(regionAngle) * 0.7;
        const ry = Math.sin(regionAngle) * 0.7;

        const podNames   = regions.get(region);
        const podCount   = podNames.length;
        const podRadius  = Math.min(0.25, 0.5 / Math.max(podCount, 1));

        podNames.forEach((name, pIdx) => {
            const podAngle = (pIdx / podCount) * 2 * Math.PI;
            pods.get(name).ndcX = rx + Math.cos(podAngle) * podRadius;
            pods.get(name).ndcY = ry + Math.sin(podAngle) * podRadius;
        });
    });
}

// ---------------------------------------------------------------------------
// PodState
// ---------------------------------------------------------------------------

/**
 * Internal state for a single pod node.
 *
 * @typedef {object} PodState
 * @property {string}  podName
 * @property {string}  region
 * @property {number}  heat         - [0, 1]
 * @property {number}  ndcX         - Normalised device coordinate X
 * @property {number}  ndcY         - Normalised device coordinate Y
 * @property {number}  phase        - Animation phase offset
 * @property {number}  activeReqs
 * @property {number}  activeCx
 */

// ---------------------------------------------------------------------------
// TrafficHeatmap
// ---------------------------------------------------------------------------

/**
 * Main heatmap renderer.
 *
 * Thread-safety note: all `updatePod` / `removePod` calls from the Web Worker
 * message handler run on the main thread (postMessage is synchronised), so
 * there is no concurrent mutation of the pod map during a render frame.
 */
export class TrafficHeatmap {
    /**
     * @param {HTMLCanvasElement} canvas
     * @param {object} [options]
     * @param {number} [options.targetFps=60]
     */
    constructor(canvas, options = {}) {
        this._canvas = canvas;
        this._targetFps = options.targetFps || 60;

        /** @type {Map<string, PodState>} */
        this._pods = new Map();

        /** Whether the pod layout needs to be recomputed before the next draw. */
        this._layoutDirty = false;

        /** Whether the GPU buffers need to be re-uploaded. */
        this._bufferDirty = false;

        this._startTime    = performance.now();
        this._rafHandle    = null;
        this._running      = false;

        this._gl           = null;
        this._nodeProgram  = null;
        this._edgeProgram  = null;

        this._nodeVao      = null;
        this._nodeVbo      = null;
        this._edgeVao      = null;
        this._edgeVbo      = null;

        /** Tooltip overlay element (optional, created if not present). */
        this._tooltip      = null;

        this._initGL();
        this._initTooltip();
        this._bindResize();
        this._bindHover();
    }

    // -----------------------------------------------------------------------
    // WebGL initialisation
    // -----------------------------------------------------------------------

    _initGL() {
        const gl = this._canvas.getContext('webgl2', {
            antialias:   false,   // MSAA disabled – we do soft discs in shader
            alpha:       true,
            premultipliedAlpha: false,
            preserveDrawingBuffer: false,
        });

        if (!gl) {
            throw new Error('WebGL2 is not available in this browser.');
        }

        this._gl = gl;

        // Compile programs
        this._nodeProgram = buildProgram(gl, VERTEX_SHADER_SRC,   FRAGMENT_SHADER_SRC);
        this._edgeProgram = buildProgram(gl, EDGE_VERT_SRC,       EDGE_FRAG_SRC);

        // Cache uniform locations
        this._uTime       = gl.getUniformLocation(this._nodeProgram, 'u_time');
        this._uResolution = gl.getUniformLocation(this._nodeProgram, 'u_resolution');

        // Node VAO + VBO
        this._nodeVao = gl.createVertexArray();
        this._nodeVbo = gl.createBuffer();
        gl.bindVertexArray(this._nodeVao);
        gl.bindBuffer(gl.ARRAY_BUFFER, this._nodeVbo);

        const stride = 4 * 4; // vec2 + float + float = 4 floats
        const aPos   = gl.getAttribLocation(this._nodeProgram, 'a_position');
        const aHeat  = gl.getAttribLocation(this._nodeProgram, 'a_heat');
        const aPhase = gl.getAttribLocation(this._nodeProgram, 'a_phase');

        gl.enableVertexAttribArray(aPos);
        gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, stride, 0);
        gl.enableVertexAttribArray(aHeat);
        gl.vertexAttribPointer(aHeat, 1, gl.FLOAT, false, stride, 8);
        gl.enableVertexAttribArray(aPhase);
        gl.vertexAttribPointer(aPhase, 1, gl.FLOAT, false, stride, 12);

        // Edge VAO + VBO
        this._edgeVao = gl.createVertexArray();
        this._edgeVbo = gl.createBuffer();
        gl.bindVertexArray(this._edgeVao);
        gl.bindBuffer(gl.ARRAY_BUFFER, this._edgeVbo);

        const eStride = 3 * 4; // vec2 + float
        const eaPos   = gl.getAttribLocation(this._edgeProgram, 'a_position');
        const eaFlow  = gl.getAttribLocation(this._edgeProgram, 'a_flow');

        gl.enableVertexAttribArray(eaPos);
        gl.vertexAttribPointer(eaPos, 2, gl.FLOAT, false, eStride, 0);
        gl.enableVertexAttribArray(eaFlow);
        gl.vertexAttribPointer(eaFlow, 1, gl.FLOAT, false, eStride, 8);

        gl.bindVertexArray(null);

        // Blending for the soft-disc glow effect.
        gl.enable(gl.BLEND);
        gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);

        this._resizeViewport();
    }

    _resizeViewport() {
        const gl = this._gl;
        const dpr = window.devicePixelRatio || 1;
        const w   = this._canvas.clientWidth  * dpr | 0;
        const h   = this._canvas.clientHeight * dpr | 0;
        if (this._canvas.width !== w || this._canvas.height !== h) {
            this._canvas.width  = w;
            this._canvas.height = h;
            gl.viewport(0, 0, w, h);
        }
    }

    _bindResize() {
        const ro = new ResizeObserver(() => {
            this._resizeViewport();
            this._layoutDirty = true;
        });
        ro.observe(this._canvas);
        this._resizeObserver = ro;
    }

    // -----------------------------------------------------------------------
    // Tooltip
    // -----------------------------------------------------------------------

    _initTooltip() {
        let tip = document.getElementById('heatmap-tooltip');
        if (!tip) {
            tip = document.createElement('div');
            tip.id = 'heatmap-tooltip';
            tip.style.cssText = [
                'position:absolute',
                'pointer-events:none',
                'background:rgba(0,0,0,0.75)',
                'color:#e2e8f0',
                'font:12px/1.5 monospace',
                'padding:6px 10px',
                'border-radius:4px',
                'border:1px solid rgba(255,255,255,0.15)',
                'display:none',
                'z-index:1000',
                'white-space:pre',
            ].join(';');
            document.body.appendChild(tip);
        }
        this._tooltip = tip;
    }

    _bindHover() {
        this._canvas.addEventListener('mousemove', (e) => {
            const rect = this._canvas.getBoundingClientRect();
            const mx   = ((e.clientX - rect.left) / rect.width)  * 2 - 1;
            const my   = -((e.clientY - rect.top)  / rect.height) * 2 + 1;

            let closest     = null;
            let closestDist = Infinity;

            for (const [, pod] of this._pods) {
                const dx = pod.ndcX - mx;
                const dy = pod.ndcY - my;
                const d  = dx * dx + dy * dy;
                if (d < closestDist) {
                    closestDist = d;
                    closest     = pod;
                }
            }

            const HOVER_THRESHOLD = 0.02; // NDC squared
            if (closest && closestDist < HOVER_THRESHOLD) {
                const pct = (closest.heat * 100).toFixed(1);
                this._tooltip.textContent =
                    `Pod:   ${closest.podName}\n` +
                    `Region: ${closest.region || 'n/a'}\n` +
                    `Heat:  ${pct}%\n` +
                    `Reqs:  ${closest.activeReqs}\n` +
                    `Cx:    ${closest.activeCx}`;
                this._tooltip.style.left    = `${e.clientX + 14}px`;
                this._tooltip.style.top     = `${e.clientY - 10}px`;
                this._tooltip.style.display = 'block';
            } else {
                this._tooltip.style.display = 'none';
            }
        });

        this._canvas.addEventListener('mouseleave', () => {
            if (this._tooltip) this._tooltip.style.display = 'none';
        });
    }

    // -----------------------------------------------------------------------
    // GPU buffer upload
    // -----------------------------------------------------------------------

    _uploadBuffers() {
        const gl   = this._gl;
        const pods = Array.from(this._pods.values());

        // --- Node buffer: [x, y, heat, phase] per pod ---
        const nodeData = new Float32Array(pods.length * 4);
        pods.forEach((pod, i) => {
            nodeData[i * 4 + 0] = pod.ndcX;
            nodeData[i * 4 + 1] = pod.ndcY;
            nodeData[i * 4 + 2] = pod.heat;
            nodeData[i * 4 + 3] = pod.phase;
        });

        gl.bindVertexArray(this._nodeVao);
        gl.bindBuffer(gl.ARRAY_BUFFER, this._nodeVbo);
        gl.bufferData(gl.ARRAY_BUFFER, nodeData, gl.DYNAMIC_DRAW);

        // --- Edge buffer: pairs of [x, y, flow] for lines between neighbours ---
        // Simple strategy: connect every pod to every other pod in the same region.
        const edgeVerts = [];
        const podArr    = pods;
        for (let i = 0; i < podArr.length; i++) {
            for (let j = i + 1; j < podArr.length; j++) {
                const a = podArr[i];
                const b = podArr[j];
                if ((a.region || 'default') !== (b.region || 'default')) continue;
                const flow = (a.heat + b.heat) * 0.5;
                edgeVerts.push(a.ndcX, a.ndcY, flow, b.ndcX, b.ndcY, flow);
            }
        }

        gl.bindVertexArray(this._edgeVao);
        gl.bindBuffer(gl.ARRAY_BUFFER, this._edgeVbo);
        gl.bufferData(
            gl.ARRAY_BUFFER,
            new Float32Array(edgeVerts),
            gl.DYNAMIC_DRAW,
        );

        gl.bindVertexArray(null);
        this._bufferDirty  = false;
        this._nodeCount    = pods.length;
        this._edgeVertCount = edgeVerts.length / 3; // floats → vertex count
    }

    // -----------------------------------------------------------------------
    // Render loop
    // -----------------------------------------------------------------------

    _drawFrame(nowMs) {
        const gl      = this._gl;
        const elapsed = (nowMs - this._startTime) / 1000.0;

        if (this._layoutDirty) {
            computeLayout(this._pods);
            this._layoutDirty = false;
            this._bufferDirty = true;
        }

        if (this._bufferDirty) {
            this._uploadBuffers();
        }

        // Clear.
        gl.clearColor(0.04, 0.06, 0.12, 1.0); // dark navy background
        gl.clear(gl.COLOR_BUFFER_BIT);

        const w = this._canvas.width;
        const h = this._canvas.height;

        // --- Draw edges ---
        if (this._edgeVertCount > 0) {
            gl.useProgram(this._edgeProgram);
            gl.bindVertexArray(this._edgeVao);
            gl.drawArrays(gl.LINES, 0, this._edgeVertCount);
        }

        // --- Draw node point-sprites ---
        if (this._nodeCount > 0) {
            gl.useProgram(this._nodeProgram);
            gl.uniform1f(this._uTime, elapsed);
            gl.uniform2f(this._uResolution, w, h);
            gl.bindVertexArray(this._nodeVao);
            gl.drawArrays(gl.POINTS, 0, this._nodeCount);
        }

        gl.bindVertexArray(null);
    }

    _scheduleFrame() {
        if (!this._running) return;

        this._rafHandle = requestAnimationFrame((ts) => {
            this._drawFrame(ts);
            this._scheduleFrame();
        });
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /**
     * Start the render loop.
     */
    start() {
        if (this._running) return;
        this._running  = true;
        this._nodeCount = 0;
        this._edgeVertCount = 0;
        this._scheduleFrame();
    }

    /**
     * Stop the render loop and release resources.
     */
    stop() {
        this._running = false;
        if (this._rafHandle !== null) {
            cancelAnimationFrame(this._rafHandle);
            this._rafHandle = null;
        }
        if (this._resizeObserver) {
            this._resizeObserver.disconnect();
        }
    }

    /**
     * Upsert a pod snapshot received from the Web Worker.
     *
     * This is the primary data-ingestion entry point.  It is intentionally
     * lightweight — no layout recomputation happens here; that is deferred to
     * the next render frame to avoid blocking the message handler.
     *
     * @param {object} snapshot  - A `PodTrafficSnapshot` from the Rust backend.
     * @param {string} snapshot.pod_name
     * @param {number} snapshot.heat_level       - [0, 1]
     * @param {number} snapshot.active_requests
     * @param {number} snapshot.active_connections
     * @param {string} [snapshot.region]
     */
    updatePod(snapshot) {
        const isNew = !this._pods.has(snapshot.pod_name);

        const existing = this._pods.get(snapshot.pod_name) || {
            podName:    snapshot.pod_name,
            region:     snapshot.region || 'default',
            phase:      Math.random() * Math.PI * 2,   // randomise pulse offset
            ndcX:       0,
            ndcY:       0,
        };

        existing.heat       = snapshot.heat_level;
        existing.activeReqs = snapshot.active_requests;
        existing.activeCx   = snapshot.active_connections;

        this._pods.set(snapshot.pod_name, existing);

        if (isNew) {
            // New pod → need full layout recompute.
            this._layoutDirty = true;
        } else {
            // Existing pod heat changed → just re-upload buffers.
            this._bufferDirty = true;
        }
    }

    /**
     * Remove a pod from the heatmap (e.g. on pod termination).
     *
     * @param {string} podName
     */
    removePod(podName) {
        if (this._pods.delete(podName)) {
            this._layoutDirty = true;
        }
    }

    /**
     * Replace the entire pod map with a fresh snapshot set.
     *
     * Useful for initial hydration or after a WebSocket reconnect.
     *
     * @param {Array<object>} snapshots
     */
    replaceAll(snapshots) {
        this._pods.clear();
        for (const s of snapshots) {
            this.updatePod(s);
        }
        this._layoutDirty = true;
    }

    /**
     * Return the current pod state map (read-only view).
     *
     * @returns {ReadonlyMap<string, PodState>}
     */
    get pods() {
        return this._pods;
    }
}

// ---------------------------------------------------------------------------
// Colour scale helper (exported for tests / legend rendering)
// ---------------------------------------------------------------------------

/**
 * Convert a normalised heat level `[0, 1]` to an RGB hex colour string using
 * the same gradient as the fragment shader.
 *
 * @param {number} t - Heat level [0, 1]
 * @returns {string} - e.g. `'#1a33ff'`
 */
export function heatToHex(t) {
    const COLD   = [0,   51,  255];
    const WARM   = [255, 204, 0  ];
    const HOT    = [255, 17,  0  ];
    const SATURE = [255, 255, 242];

    function lerp(a, b, x) {
        return a.map((v, i) => Math.round(v + (b[i] - v) * x));
    }

    let rgb;
    if (t < 0.5) {
        rgb = lerp(COLD, WARM, t * 2);
    } else if (t < 0.85) {
        rgb = lerp(WARM, HOT, (t - 0.5) / 0.35);
    } else {
        rgb = lerp(HOT, SATURE, (t - 0.85) / 0.15);
    }

    return '#' + rgb.map(v => v.toString(16).padStart(2, '0')).join('');
}
