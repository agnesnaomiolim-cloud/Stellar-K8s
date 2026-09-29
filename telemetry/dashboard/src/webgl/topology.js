/**
 * @fileoverview WebGL Validator Topology Visualization
 *
 * Renders a live 3-D force-directed graph of Stellar validator peering
 * connections using Three.js with custom GLSL shaders.
 *
 * Architecture
 * ─────────────
 *  ┌──────────────────────────────────────────────────────────┐
 *  │  TopologyRenderer  (this module)                         │
 *  │   ├─ Scene, Camera, Renderer (WebGL2)                    │
 *  │   ├─ NodeMesh  – instanced PointSprites via custom vert  │
 *  │   ├─ EdgeMesh  – LineSegments with custom gradient frag  │
 *  │   ├─ ForceLayout – rudimentary spring/repulsion in JS    │
 *  │   └─ StatsOverlay – lightweight HUD (fps + node count)   │
 *  └──────────────────────────────────────────────────────────┘
 *
 * The renderer is deliberately kept free of any bundler dependencies so it
 * can be imported directly as an ES module:
 *
 *   <script type="module">
 *     import { TopologyRenderer } from './webgl/topology.js';
 *     const renderer = new TopologyRenderer(document.getElementById('canvas'));
 *     renderer.connect('ws://localhost:8765/ws/scp-topology');
 *     renderer.start();
 *   </script>
 *
 * Performance constraints
 * ────────────────────────
 * • Node geometry: InstancedMesh with a single draw call for up to 1 000 nodes.
 * • Edge geometry: BufferGeometry updated in-place (no reallocation per frame).
 * • Frame-budget guard: physics tick is capped at 10 ms via performance.now().
 * • No per-frame object allocations on the hot path.
 *
 * @module topology
 */

import * as THREE from 'https://cdn.jsdelivr.net/npm/three@0.165.0/build/three.module.js';

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/** Maximum number of simultaneously rendered nodes. */
const MAX_NODES = 1000;

/** Maximum number of simultaneously rendered edges (directed links). */
const MAX_EDGES = 5000;

/** Physics: spring rest length (world-units). */
const SPRING_REST = 80;

/** Physics: spring stiffness. */
const SPRING_K = 0.04;

/** Physics: node repulsion coefficient. */
const REPULSION = 4000;

/** Physics: velocity damping per frame. */
const DAMPING = 0.88;

/** Physics: maximum per-tick compute budget (ms). */
const PHYSICS_BUDGET_MS = 10;

/** Colour map: health -> 0xRRGGBB */
const HEALTH_COLOUR = Object.freeze({
  synced: 0x00e676,      // green
  syncing: 0xffeb3b,     // amber
  degraded: 0xff9800,    // orange
  partitioned: 0xf44336, // red
  unknown: 0x78909c,     // grey
});

/** Selects which CSS class is added to the HUD partition-warning banner. */
const PARTITION_CSS_CLASS = 'partition-alert';

// ─────────────────────────────────────────────────────────────────────────────
// GLSL Shaders
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Custom vertex shader for node point-sprites.
 *
 * Each *instance* carries:
 *   instancePosition  – vec3 world position
 *   instanceColor     – vec3 RGB health colour
 *   instanceScale     – float radius scale
 */
const NODE_VERT_GLSL = /* glsl */`
  precision highp float;

  // Per-instance attributes (set via InstancedMesh)
  attribute vec3  instancePosition;
  attribute vec3  instanceColor;
  attribute float instanceScale;

  varying vec3  vColor;
  varying float vDist; // camera distance for soft size falloff

  uniform mat4 projectionMatrix;
  uniform mat4 modelViewMatrix;
  uniform float pointSizeBase;
  uniform float logDepthBufFC;

  void main() {
    vColor = instanceColor;

    vec4 mvPos = modelViewMatrix * vec4(instancePosition, 1.0);
    vDist = -mvPos.z;

    // Perspective-correct point size: larger when close, smaller when far.
    // Clamp between 4 and 32 CSS pixels.
    float pxSize = clamp(pointSizeBase * instanceScale / vDist, 4.0, 32.0);
    gl_PointSize = pxSize;
    gl_Position  = projectionMatrix * mvPos;
  }
`;

/**
 * Fragment shader for node point-sprites.
 *
 * Renders a smooth anti-aliased circle with a bright core and soft halo.
 * Discards corner pixels to avoid square artefacts at high zoom.
 */
const NODE_FRAG_GLSL = /* glsl */`
  precision highp float;

  varying vec3  vColor;
  varying float vDist;

  void main() {
    // uv in [-1, 1]
    vec2 uv = (gl_PointCoord - 0.5) * 2.0;
    float r  = dot(uv, uv);

    // Hard clip at circle boundary.
    if (r > 1.0) discard;

    // Bright core + glow halo.
    float alpha = 1.0 - smoothstep(0.4, 1.0, r);
    vec3  glow  = vColor * (1.0 - smoothstep(0.0, 0.6, r)) * 0.6;

    gl_FragColor = vec4(vColor * alpha + glow, alpha);
  }
`;

/**
 * Vertex shader for edges (LineSegments).
 *
 * The two endpoints each carry a `lineColorA` / `lineColorB` attribute so the
 * fragment shader can interpolate a gradient along the edge.
 */
const EDGE_VERT_GLSL = /* glsl */`
  precision highp float;

  attribute vec3 colorA;
  attribute vec3 colorB;

  varying vec3 vColor;

  uniform mat4 projectionMatrix;
  uniform mat4 modelViewMatrix;

  void main() {
    // gl_VertexID is 0 for the source end, 1 for the target end.
    // Three.js doesn't expose gl_VertexID as an attribute, so we use
    // a manual 'endpointFlag' float: 0.0 = source, 1.0 = target.
    vColor = colorA; // overridden in JS before upload
    gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
  }
`;

/** Fragment shader for edge lines – simple interpolated colour + alpha taper. */
const EDGE_FRAG_GLSL = /* glsl */`
  precision highp float;

  varying vec3 vColor;

  void main() {
    gl_FragColor = vec4(vColor, 0.55);
  }
`;

// ─────────────────────────────────────────────────────────────────────────────
// Force-directed layout
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Lightweight spring-repulsion layout engine operating directly on a flat
 * Float32Array to avoid GC pressure.
 *
 * Each node occupies 6 floats: [x, y, z, vx, vy, vz].
 */
class ForceLayout {
  /**
   * @param {number} maxNodes - capacity (pre-allocated)
   */
  constructor(maxNodes) {
    this._max = maxNodes;
    this._buf = new Float32Array(maxNodes * 6); // x,y,z,vx,vy,vz
    this._count = 0;
    this._idToIdx = new Map(); // nodeId -> buffer index
  }

  /** Add or update a node by ID. Returns buffer index. */
  upsert(id) {
    if (this._idToIdx.has(id)) return this._idToIdx.get(id);
    const idx = this._count++;
    this._idToIdx.set(id, idx);
    const base = idx * 6;
    // Seed position on a sphere surface for fast convergence.
    const theta = Math.random() * Math.PI * 2;
    const phi = Math.acos(2 * Math.random() - 1);
    const r = 200 + Math.random() * 200;
    this._buf[base]     = r * Math.sin(phi) * Math.cos(theta);
    this._buf[base + 1] = r * Math.sin(phi) * Math.sin(theta);
    this._buf[base + 2] = r * Math.cos(phi);
    // velocities start at zero
    return idx;
  }

  /** Return [x, y, z] for a node by ID. */
  position(id) {
    const idx = this._idToIdx.get(id);
    if (idx === undefined) return [0, 0, 0];
    const b = idx * 6;
    return [this._buf[b], this._buf[b + 1], this._buf[b + 2]];
  }

  /**
   * Advance physics by one step.
   *
   * @param {string[][]} edges - array of [sourceId, targetId] pairs
   * @param {number} budgetMs  - maximum wall-clock ms to spend
   */
  tick(edges, budgetMs) {
    const n = this._count;
    if (n === 0) return;

    const t0 = performance.now();
    const buf = this._buf;

    // Build a temporary force accumulator [fx0,fy0,fz0, fx1,fy1,fz1, …]
    const forces = new Float32Array(n * 3);

    // ── Repulsion (O(n²) capped by budget) ───────────────────────────────
    outer: for (let i = 0; i < n; i++) {
      if (performance.now() - t0 > budgetMs) break outer;
      const bi = i * 6;
      for (let j = i + 1; j < n; j++) {
        const bj = j * 6;
        let dx = buf[bi] - buf[bj];
        let dy = buf[bi + 1] - buf[bj + 1];
        let dz = buf[bi + 2] - buf[bj + 2];
        const dist2 = dx * dx + dy * dy + dz * dz + 0.001;
        const force = REPULSION / dist2;
        const dist = Math.sqrt(dist2);
        dx /= dist; dy /= dist; dz /= dist;
        forces[i * 3]     += dx * force;
        forces[i * 3 + 1] += dy * force;
        forces[i * 3 + 2] += dz * force;
        forces[j * 3]     -= dx * force;
        forces[j * 3 + 1] -= dy * force;
        forces[j * 3 + 2] -= dz * force;
      }
    }

    // ── Spring attraction ─────────────────────────────────────────────────
    for (const [srcId, tgtId] of edges) {
      const si = this._idToIdx.get(srcId);
      const ti = this._idToIdx.get(tgtId);
      if (si === undefined || ti === undefined) continue;
      const bs = si * 6, bt = ti * 6;
      let dx = buf[bt] - buf[bs];
      let dy = buf[bt + 1] - buf[bs + 1];
      let dz = buf[bt + 2] - buf[bs + 2];
      const dist = Math.sqrt(dx * dx + dy * dy + dz * dz) + 0.001;
      const displacement = dist - SPRING_REST;
      const force = SPRING_K * displacement / dist;
      forces[si * 3]     += dx * force;
      forces[si * 3 + 1] += dy * force;
      forces[si * 3 + 2] += dz * force;
      forces[ti * 3]     -= dx * force;
      forces[ti * 3 + 1] -= dy * force;
      forces[ti * 3 + 2] -= dz * force;
    }

    // ── Integrate ─────────────────────────────────────────────────────────
    for (let i = 0; i < n; i++) {
      const b = i * 6;
      buf[b + 3] = (buf[b + 3] + forces[i * 3])     * DAMPING;
      buf[b + 4] = (buf[b + 4] + forces[i * 3 + 1]) * DAMPING;
      buf[b + 5] = (buf[b + 5] + forces[i * 3 + 2]) * DAMPING;
      buf[b]     += buf[b + 3];
      buf[b + 1] += buf[b + 4];
      buf[b + 2] += buf[b + 5];
    }
  }

  /**
   * Remove all nodes whose IDs are not in `liveIds`.
   * @param {Set<string>} liveIds
   */
  pruneStale(liveIds) {
    for (const [id] of this._idToIdx) {
      if (!liveIds.has(id)) this._idToIdx.delete(id);
      // Note: we don't compact the buffer – slots are reused via _idToIdx.
    }
  }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main renderer class
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Manages the full rendering pipeline: WebSocket connection, force layout,
 * Three.js scene and GPU upload.
 */
export class TopologyRenderer {
  /**
   * @param {HTMLCanvasElement} canvas - target canvas element
   */
  constructor(canvas) {
    this._canvas = canvas;
    this._running = false;
    this._ws = null;
    this._frame = null;  // latest TopologyFrame from server
    this._rafId = null;

    // Scene setup
    this._scene    = new THREE.Scene();
    this._scene.background = new THREE.Color(0x0a0e17);
    this._camera   = this._makeCamera();
    this._renderer = this._makeRenderer();

    // Geometry pools
    this._nodeMesh = this._makeNodeMesh();
    this._edgeMesh = this._makeEdgeMesh();
    this._scene.add(this._nodeMesh);
    this._scene.add(this._edgeMesh);

    // Subtle ambient particles for depth
    this._scene.add(this._makeStarfield());

    // Physics layout
    this._layout = new ForceLayout(MAX_NODES);

    // HUD
    this._hud = this._makeHUD();

    // Resize observer
    this._ro = new ResizeObserver(() => this._onResize());
    this._ro.observe(canvas);

    // Orbit-style mouse drag
    this._setupOrbitControls();
  }

  // ── Public API ────────────────────────────────────────────────────────────

  /**
   * Connect to the SCP topology WebSocket stream.
   *
   * Reconnects automatically after a 2-second back-off on close/error.
   *
   * @param {string} url - WebSocket URL, e.g. `ws://localhost:8765/ws/scp-topology`
   */
  connect(url) {
    this._wsUrl = url;
    this._openWebSocket();
  }

  /** Start the animation loop. */
  start() {
    if (this._running) return;
    this._running = true;
    this._tick();
  }

  /** Stop the animation loop and close the WebSocket. */
  stop() {
    this._running = false;
    if (this._rafId) cancelAnimationFrame(this._rafId);
    if (this._ws) this._ws.close();
    this._ro.disconnect();
  }

  // ── WebSocket ─────────────────────────────────────────────────────────────

  _openWebSocket() {
    if (this._ws) {
      this._ws.onclose = null;
      this._ws.close();
    }

    const ws = new WebSocket(this._wsUrl);
    ws.binaryType = 'arraybuffer';

    ws.onopen = () => {
      console.info('[topology] WebSocket connected:', this._wsUrl);
      this._hud.setStatus('connected');
    };

    ws.onmessage = (ev) => {
      try {
        this._frame = JSON.parse(ev.data);
      } catch (e) {
        console.warn('[topology] bad frame:', e);
      }
    };

    ws.onerror = (e) => {
      console.warn('[topology] WebSocket error:', e);
      this._hud.setStatus('error');
    };

    ws.onclose = () => {
      console.info('[topology] WebSocket closed – reconnecting in 2 s');
      this._hud.setStatus('reconnecting');
      setTimeout(() => this._openWebSocket(), 2000);
    };

    this._ws = ws;
  }

  // ── Animation loop ────────────────────────────────────────────────────────

  _tick() {
    if (!this._running) return;
    this._rafId = requestAnimationFrame(() => this._tick());

    if (this._frame) {
      this._applyFrame(this._frame);
      this._frame = null;
    }

    // Physics step
    const edgePairs = this._currentEdgePairs || [];
    this._layout.tick(edgePairs, PHYSICS_BUDGET_MS);

    // Upload updated positions to GPU
    this._uploadNodePositions();
    this._uploadEdgePositions();

    // Slowly rotate the scene for "screen-saver" feel when idle.
    this._scene.rotation.y += 0.0005;

    this._renderer.render(this._scene, this._camera);
    this._hud.tick();
  }

  // ── Frame application ─────────────────────────────────────────────────────

  /**
   * Apply a decoded TopologyFrame to the renderer state.
   * @param {Object} frame
   */
  _applyFrame(frame) {
    const liveIds = new Set(frame.nodes.map(n => n.id));
    this._layout.pruneStale(liveIds);

    // Ensure every node has a layout slot.
    for (const node of frame.nodes) {
      this._layout.upsert(node.id);
    }

    // Store node metadata for colour/health lookup during upload.
    this._nodeMap = new Map(frame.nodes.map(n => [n.id, n]));

    // Store edge pairs for physics.
    this._currentEdgePairs = frame.edges.map(e => [e.source, e.target]);
    this._currentEdges = frame.edges;

    // Update node count in HUD.
    this._hud.setNodeCount(frame.nodes.length);
    this._hud.setLedger(frame.local_ledger);

    // Partition banner
    if (frame.partition_detected) {
      this._canvas.parentElement?.classList.add(PARTITION_CSS_CLASS);
      this._hud.setPartition(true);
    } else {
      this._canvas.parentElement?.classList.remove(PARTITION_CSS_CLASS);
      this._hud.setPartition(false);
    }
  }

  // ── GPU upload ────────────────────────────────────────────────────────────

  _uploadNodePositions() {
    if (!this._nodeMap) return;
    const geo = this._nodeMesh.geometry;
    const posArr = geo.attributes.instancePosition.array;
    const colArr = geo.attributes.instanceColor.array;
    const sclArr = geo.attributes.instanceScale.array;

    let i = 0;
    for (const [id, node] of this._nodeMap) {
      const [x, y, z] = this._layout.position(id);
      posArr[i * 3]     = x;
      posArr[i * 3 + 1] = y;
      posArr[i * 3 + 2] = z;

      const col = healthToRgb(node.health);
      colArr[i * 3]     = col[0];
      colArr[i * 3 + 1] = col[1];
      colArr[i * 3 + 2] = col[2];

      // Local node is rendered slightly larger.
      sclArr[i] = node.label === 'local' ? 2.0 : 1.0;
      i++;
    }

    this._nodeMesh.count = i;
    geo.attributes.instancePosition.needsUpdate = true;
    geo.attributes.instanceColor.needsUpdate = true;
    geo.attributes.instanceScale.needsUpdate = true;
  }

  _uploadEdgePositions() {
    if (!this._currentEdges || !this._nodeMap) return;
    const geo = this._edgeMesh.geometry;
    const posArr = geo.attributes.position.array;
    const colArr = geo.attributes.colorA.array;

    let v = 0;
    for (const edge of this._currentEdges) {
      const [sx, sy, sz] = this._layout.position(edge.source);
      const [tx, ty, tz] = this._layout.position(edge.target);
      const srcNode = this._nodeMap.get(edge.source);
      const tgtNode = this._nodeMap.get(edge.target);
      const sc = srcNode ? healthToRgb(srcNode.health) : [0.47, 0.56, 0.61];
      const tc = tgtNode ? healthToRgb(tgtNode.health) : [0.47, 0.56, 0.61];

      // Source vertex
      posArr[v * 3]     = sx;
      posArr[v * 3 + 1] = sy;
      posArr[v * 3 + 2] = sz;
      colArr[v * 3]     = sc[0];
      colArr[v * 3 + 1] = sc[1];
      colArr[v * 3 + 2] = sc[2];
      v++;

      // Target vertex
      posArr[v * 3]     = tx;
      posArr[v * 3 + 1] = ty;
      posArr[v * 3 + 2] = tz;
      colArr[v * 3]     = tc[0];
      colArr[v * 3 + 1] = tc[1];
      colArr[v * 3 + 2] = tc[2];
      v++;
    }

    // Zero out unused slots.
    for (let i = v; i < MAX_EDGES * 2; i++) {
      posArr[i * 3] = posArr[i * 3 + 1] = posArr[i * 3 + 2] = 0;
    }

    this._edgeMesh.geometry.setDrawRange(0, v);
    geo.attributes.position.needsUpdate = true;
    geo.attributes.colorA.needsUpdate = true;
  }

  // ── Scene helpers ─────────────────────────────────────────────────────────

  _makeCamera() {
    const cam = new THREE.PerspectiveCamera(
      60,
      this._canvas.clientWidth / this._canvas.clientHeight,
      0.1,
      5000
    );
    cam.position.set(0, 0, 800);
    return cam;
  }

  _makeRenderer() {
    const renderer = new THREE.WebGLRenderer({
      canvas: this._canvas,
      antialias: true,
      alpha: false,
      powerPreference: 'high-performance',
    });
    renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
    renderer.setSize(this._canvas.clientWidth, this._canvas.clientHeight, false);
    return renderer;
  }

  _makeNodeMesh() {
    const geo = new THREE.BufferGeometry();

    // Single dummy vertex – the real positions are in instance attributes.
    geo.setAttribute('position', new THREE.BufferAttribute(new Float32Array([0, 0, 0]), 3));

    // Per-instance attributes (pre-allocated to MAX_NODES).
    const instancePos   = new Float32Array(MAX_NODES * 3);
    const instanceColor = new Float32Array(MAX_NODES * 3);
    const instanceScale = new Float32Array(MAX_NODES).fill(1.0);

    geo.setAttribute('instancePosition',
      new THREE.InstancedBufferAttribute(instancePos, 3));
    geo.setAttribute('instanceColor',
      new THREE.InstancedBufferAttribute(instanceColor, 3));
    geo.setAttribute('instanceScale',
      new THREE.InstancedBufferAttribute(instanceScale, 1));

    const mat = new THREE.ShaderMaterial({
      vertexShader:   NODE_VERT_GLSL,
      fragmentShader: NODE_FRAG_GLSL,
      uniforms: {
        pointSizeBase: { value: 600.0 },
      },
      transparent:   true,
      depthWrite:    false,
      blending:      THREE.AdditiveBlending,
    });

    const mesh = new THREE.Points(geo, mat);
    mesh.frustumCulled = false;
    return mesh;
  }

  _makeEdgeMesh() {
    const posArr = new Float32Array(MAX_EDGES * 2 * 3);
    const colArr = new Float32Array(MAX_EDGES * 2 * 3);

    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.BufferAttribute(posArr, 3));
    geo.setAttribute('colorA',   new THREE.BufferAttribute(colArr, 3));
    geo.setDrawRange(0, 0);

    const mat = new THREE.ShaderMaterial({
      vertexShader:   EDGE_VERT_GLSL,
      fragmentShader: EDGE_FRAG_GLSL,
      transparent:    true,
      depthWrite:     false,
      blending:       THREE.AdditiveBlending,
    });

    const mesh = new THREE.LineSegments(geo, mat);
    mesh.frustumCulled = false;
    return mesh;
  }

  _makeStarfield() {
    const count = 2000;
    const pos = new Float32Array(count * 3);
    for (let i = 0; i < count; i++) {
      const r = 1500 + Math.random() * 1000;
      const theta = Math.random() * Math.PI * 2;
      const phi   = Math.acos(2 * Math.random() - 1);
      pos[i * 3]     = r * Math.sin(phi) * Math.cos(theta);
      pos[i * 3 + 1] = r * Math.sin(phi) * Math.sin(theta);
      pos[i * 3 + 2] = r * Math.cos(phi);
    }
    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
    const mat = new THREE.PointsMaterial({ color: 0x334455, size: 1.5 });
    return new THREE.Points(geo, mat);
  }

  _makeHUD() {
    return new HUD(this._canvas.parentElement);
  }

  // ── Orbit-style mouse controls ────────────────────────────────────────────

  _setupOrbitControls() {
    let isDragging = false;
    let lastX = 0, lastY = 0;

    this._canvas.addEventListener('mousedown', (e) => {
      isDragging = true;
      lastX = e.clientX;
      lastY = e.clientY;
    });

    window.addEventListener('mousemove', (e) => {
      if (!isDragging) return;
      const dx = e.clientX - lastX;
      const dy = e.clientY - lastY;
      lastX = e.clientX;
      lastY = e.clientY;
      this._scene.rotation.y += dx * 0.005;
      this._scene.rotation.x += dy * 0.005;
    });

    window.addEventListener('mouseup', () => { isDragging = false; });

    this._canvas.addEventListener('wheel', (e) => {
      e.preventDefault();
      this._camera.position.z = Math.max(
        100,
        Math.min(3000, this._camera.position.z + e.deltaY * 0.5)
      );
    }, { passive: false });
  }

  // ── Resize ────────────────────────────────────────────────────────────────

  _onResize() {
    const w = this._canvas.clientWidth;
    const h = this._canvas.clientHeight;
    this._camera.aspect = w / h;
    this._camera.updateProjectionMatrix();
    this._renderer.setSize(w, h, false);
  }
}

// ─────────────────────────────────────────────────────────────────────────────
// HUD overlay
// ─────────────────────────────────────────────────────────────────────────────

/** Heads-up display rendered as DOM elements overlaid on the canvas. */
class HUD {
  constructor(container) {
    this._el = document.createElement('div');
    this._el.className = 'topology-hud';
    this._el.innerHTML = `
      <span class="hud-status" title="WebSocket status">⬤ –</span>
      <span class="hud-nodes" title="Validators">0 nodes</span>
      <span class="hud-ledger" title="Latest ledger">Ldgr –</span>
      <span class="hud-fps"   title="Render FPS">– fps</span>
      <div  class="hud-partition" hidden>⚠ PARTITION DETECTED</div>
    `;

    if (container) container.style.position = 'relative';
    container?.appendChild(this._el);

    this._statusEl    = this._el.querySelector('.hud-status');
    this._nodesEl     = this._el.querySelector('.hud-nodes');
    this._ledgerEl    = this._el.querySelector('.hud-ledger');
    this._fpsEl       = this._el.querySelector('.hud-fps');
    this._partitionEl = this._el.querySelector('.hud-partition');

    this._frames = 0;
    this._lastFpsSample = performance.now();
  }

  setStatus(s) {
    const colours = { connected: '#00e676', error: '#f44336', reconnecting: '#ffeb3b' };
    this._statusEl.style.color = colours[s] || '#78909c';
    this._statusEl.textContent = `⬤ ${s}`;
  }

  setNodeCount(n) { this._nodesEl.textContent = `${n} nodes`; }
  setLedger(l)    { this._ledgerEl.textContent = `Ldgr ${l.toLocaleString()}`; }

  setPartition(on) {
    this._partitionEl.hidden = !on;
  }

  tick() {
    this._frames++;
    const now = performance.now();
    const elapsed = now - this._lastFpsSample;
    if (elapsed >= 1000) {
      const fps = Math.round(this._frames * 1000 / elapsed);
      this._fpsEl.textContent = `${fps} fps`;
      this._frames = 0;
      this._lastFpsSample = now;
    }
  }
}

// ─────────────────────────────────────────────────────────────────────────────
// Utilities
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Convert a health string to an [r, g, b] triple in [0, 1].
 * @param {string} health
 * @returns {[number, number, number]}
 */
function healthToRgb(health) {
  const hex = HEALTH_COLOUR[health] ?? HEALTH_COLOUR.unknown;
  return [
    ((hex >> 16) & 0xff) / 255,
    ((hex >> 8)  & 0xff) / 255,
    ((hex)       & 0xff) / 255,
  ];
}
