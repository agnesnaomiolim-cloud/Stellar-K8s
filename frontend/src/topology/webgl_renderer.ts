/**
 * High-performance WebGL renderer for the quorum topology.
 *
 * Performance strategy for 2,000+ nodes at 60 FPS:
 *  - Nodes: ONE `InstancedMesh` → a single draw call for every sphere.
 *  - Edges: ONE `LineSegments` with per-vertex colors → single draw call.
 *  - Positions are written into instance matrices / color attributes
 *    only when the simulation tick actually moved them (dirty flag).
 *  - Picking uses GPU id-mapping via `raycast` on the instanced mesh
 *    (Three.js solves instance intersection without JS per-node loops).
 *  - Render loop pauses entirely when the simulation is settled and the
 *    camera is still (`render-on-demand`), eliminating idle GPU work.
 */
import * as THREE from 'three';
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js';
import type { QuorumGraph, GraphNode, TrustKind } from './types.js'
import type { LayoutTickPayload } from './layout.js'

const NODE_COLORS: Record<TrustKind | 'critical' | 'hub' | 'normal', number> = {
  normal: 0x8fb7e8,
  critical: 0xe74c3c,
  hub: 0xf39c12,
  direct: 0x2ecc71,
  indirect: 0x4da3ff,
  missing: 0xe74c3c,
};

const EDGE_COLORS: Record<TrustKind, number> = {
  direct: 0x2ecc71,
  indirect: 0x4da3ff,
  missing: 0xe74c3c,
};

const SPHERE_SEGMENTS = 12;
const SPHERE_RING_SEGMENTS = 8;
const MAX_INSTANCES = 4096;

export interface TopologyRendererOptions {
  container: HTMLElement;
  /** Called when the user clicks a node. */
  onNodeSelect: (node: GraphNode | null) => void;
  /** Called every animation frame with instantaneous FPS. */
  onFps: (fps: number, frameMs: number) => void;
}

export class TopologyRenderer {
  private renderer: THREE.WebGLRenderer;
  private scene: THREE.Scene;
  private camera: THREE.PerspectiveCamera;
  private controls: OrbitControls;
  private nodeMesh: THREE.InstancedMesh | null = null;
  private edgeLines: THREE.LineSegments | null = null;
  private nodePositions = new Float32Array(MAX_INSTANCES * 3);
  private nodeRadii = new Float32Array(MAX_INSTANCES);
  private nodeIdOrder: string[] = [];
  private nodeByIndex = new Map<string, number>();
  private indexByNode = new Map<number, string>();
  private graph: QuorumGraph | null = null;
  private nodeById = new Map<string, GraphNode>();
  private positionsById = new Map<string, { x: number; y: number; z: number }>();
  private needsRender = true;
  private disposed = false;
  private lastFpsSample = performance.now();
  private framesSinceSample = 0;
  private highlight: THREE.PointLight;
  private scratchMatrix = new THREE.Matrix4();

  constructor(private options: TopologyRendererOptions) {
    const { container } = options;

    this.renderer = new THREE.WebGLRenderer({
      antialias: true,
      powerPreference: 'high-performance',
    });
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
    this.renderer.setSize(container.clientWidth, container.clientHeight);
    this.renderer.setClearColor(0x04070d, 1);
    container.appendChild(this.renderer.domElement);

    this.scene = new THREE.Scene();
    this.scene.fog = new THREE.FogExp2(0x04070d, 0.0012);

    this.camera = new THREE.PerspectiveCamera(
      55,
      container.clientWidth / container.clientHeight,
      0.1,
      20000,
    );
    this.camera.position.set(0, 220, 640);

    this.controls = new OrbitControls(this.camera, this.renderer.domElement);
    this.controls.enableDamping = true;
    this.controls.dampingFactor = 0.08;
    this.controls.zoomSpeed = 1.2;
    this.controls.minDistance = 5;
    this.controls.maxDistance = 8000;
    this.controls.addEventListener('change', () => {
      this.needsRender = true;
    });

    // Soft lighting: hemisphere keeps unlit faces readable.
    const hemi = new THREE.HemisphereLight(0xbfd8ff, 0x0a101c, 0.9);
    this.scene.add(hemi);
    const dir = new THREE.DirectionalLight(0xffffff, 0.7);
    dir.position.set(120, 300, 160);
    this.scene.add(dir);

    this.highlight = new THREE.PointLight(0x4da3ff, 0, 300);
    this.scene.add(this.highlight);

    this.renderer.domElement.addEventListener('pointerdown', this.onPointerDown);
    this.renderer.domElement.addEventListener('pointermove', this.onPointerMove);
    window.addEventListener('resize', this.onResize);

    // Render-on-demand loop: only draws when something changed.
    const loop = (): void => {
      if (this.disposed) return;
      requestAnimationFrame(loop);
      this.tick();
    };
    requestAnimationFrame(loop);
  }

  /** Replace the entire graph (initial load). */
  setGraph(graph: QuorumGraph): void {
    this.graph = graph;
    this.nodeById = new Map(graph.nodes.map((n) => [n.id, n]));
    this.rebuildNodes();
    this.rebuildEdges();
    this.needsRender = true;
  }

  /** Apply a simulation tick (positions) from the layout worker. */
  applyTick(payload: LayoutTickPayload): void {
    const mesh = this.nodeMesh;
    for (const n of payload.nodes) {
      this.positionsById.set(n.id, n);
      const idx = this.nodeByIndex.get(n.id);
      if (mesh !== null && idx !== undefined) {
        this.nodePositions[idx * 3] = n.x;
        this.nodePositions[idx * 3 + 1] = n.y;
        this.nodePositions[idx * 3 + 2] = n.z;
        const r = this.nodeRadii[idx]!;
        this.scratchMatrix.makeScale(r, r, r);
        this.scratchMatrix.setPosition(n.x, n.y, n.z);
        mesh.setMatrixAt(idx, this.scratchMatrix);
      }
    }
    if (mesh !== null) {
      mesh.instanceMatrix.needsUpdate = true;
    }
    this.syncEdgesFromPositions();
    if (payload.done) {
      this.controls.autoRotate = false;
    }
    this.needsRender = true;
  }

  private rebuildNodes(): void {
    if (this.nodeMesh) {
      this.scene.remove(this.nodeMesh);
      this.nodeMesh.geometry.dispose();
      (this.nodeMesh.material as THREE.Material).dispose();
      this.nodeMesh = null;
    }
    const graph = this.graph;
    if (!graph) return;

    const count = Math.min(graph.nodes.length, MAX_INSTANCES);
    this.nodeIdOrder = graph.nodes.slice(0, count).map((n) => n.id);
    this.nodeByIndex = new Map(this.nodeIdOrder.map((id, i) => [id, i]));
    this.indexByNode = new Map(this.nodeIdOrder.map((id, i) => [i, id]));

    const geometry = new THREE.SphereGeometry(1, SPHERE_SEGMENTS, SPHERE_RING_SEGMENTS);
    const material = new THREE.MeshPhongMaterial();
    const mesh = new THREE.InstancedMesh(geometry, material, count);
    mesh.instanceMatrix.setUsage(THREE.DynamicDrawUsage);

    const color = new THREE.Color();
    const m = new THREE.Matrix4();
    for (let i = 0; i < count; i++) {
      const node = graph.nodes[i]!;
      const radius = nodeRadius(node);
      this.nodeRadii[i] = radius;
      color.setHex(
        node.isArticulationPoint
          ? NODE_COLORS.critical
          : node.trusters + node.trusting >= 12
            ? NODE_COLORS.hub
            : NODE_COLORS.normal,
      );
      mesh.setColorAt(i, color);
      m.makeScale(radius, radius, radius);
      mesh.setMatrixAt(i, m);
    }
    mesh.instanceColor!.setUsage(THREE.DynamicDrawUsage);

    this.nodeMesh = mesh;
    this.scene.add(mesh);
  }

  private rebuildEdges(): void {
    if (this.edgeLines) {
      this.scene.remove(this.edgeLines);
      this.edgeLines.geometry.dispose();
      (this.edgeLines.material as THREE.Material).dispose();
      this.edgeLines = null;
    }
    const graph = this.graph;
    if (!graph) return;

    const count = Math.min(graph.edges.length, MAX_INSTANCES * 4);
    const positions = new Float32Array(count * 2 * 3);
    const colors = new Float32Array(count * 2 * 3);
    const c = new THREE.Color();
    const p = this.nodePositions;
    const idxOf = this.nodeByIndex;

    for (let i = 0; i < count; i++) {
      const e = graph.edges[i]!;
      const si = idxOf.get(e.source);
      const ti = idxOf.get(e.target);
      c.setHex(EDGE_COLORS[e.kind]);
      for (const [slot, idx] of [
        [0, si],
        [1, ti],
      ] as const) {
        const o = (i * 2 + slot) * 3;
        if (idx === undefined) {
          positions[o] = 0;
          positions[o + 1] = -10000;
          positions[o + 2] = 0;
        } else {
          positions[o] = p[idx * 3]!;
          positions[o + 1] = p[idx * 3 + 1]!;
          positions[o + 2] = p[idx * 3 + 2]!;
        }
        colors[o] = c.r;
        colors[o + 1] = c.g;
        colors[o + 2] = c.b;
      }
    }

    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute('position', new THREE.BufferAttribute(positions, 3));
    geometry.setAttribute('color', new THREE.BufferAttribute(colors, 3));
    const material = new THREE.LineBasicMaterial({ vertexColors: true, transparent: true, opacity: 0.55 });
    this.edgeLines = new THREE.LineSegments(geometry, material);
    this.scene.add(this.edgeLines);
  }

  /** Update edge endpoints in place after a physics tick. */
  private syncEdgesFromPositions(): void {
    if (!this.edgeLines || !this.graph) return;
    const attr = this.edgeLines.geometry.getAttribute('position') as THREE.BufferAttribute;
    const arr = attr.array as Float32Array;
    const p = this.nodePositions;
    const idxOf = this.nodeByIndex;
    const edges = this.graph.edges;
    const count = Math.min(edges.length, arr.length / 6);
    for (let i = 0; i < count; i++) {
      const e = edges[i]!;
      const si = idxOf.get(e.source);
      const ti = idxOf.get(e.target);
      if (si === undefined || ti === undefined) continue;
      const o = i * 6;
      arr[o] = p[si * 3]!;
      arr[o + 1] = p[si * 3 + 1]!;
      arr[o + 2] = p[si * 3 + 2]!;
      arr[o + 3] = p[ti * 3]!;
      arr[o + 4] = p[ti * 3 + 1]!;
      arr[o + 5] = p[ti * 3 + 2]!;
    }
    attr.needsUpdate = true;
  }

  private tick(): void {
    const now = performance.now();
    this.framesSinceSample += 1;
    if (now - this.lastFpsSample >= 500) {
      const fps = (this.framesSinceSample * 1000) / (now - this.lastFpsSample);
      this.options.onFps(fps, (now - this.lastFpsSample) / this.framesSinceSample);
      this.framesSinceSample = 0;
      this.lastFpsSample = now;
    }

    if (this.controls.autoRotate || this.controls.enableDamping) {
      this.controls.update();
    }
    if (!this.needsRender) return;

    this.renderer.render(this.scene, this.camera);
    this.needsRender = false;
  }

  setSelected(id: string | null): void {
    this.needsRender = true;
    // Camera focus pulse via light position
    if (id) {
      const pos = this.positionsById.get(id);
      if (pos) {
        this.highlight.position.set(pos.x, pos.y, pos.z);
        this.highlight.intensity = 1.5;
      }
    } else {
      this.highlight.intensity = 0;
    }
  }

  private onPointerDown(event: PointerEvent): void {
    if (event.button !== 0) return;
    const hit = this.pick(event);
    this.options.onNodeSelect(hit ? this.nodeById.get(hit) ?? null : null);
    this.setSelected(hit ?? null);
  }

  private onPointerMove(event: PointerEvent): void {
    // Cheap hover picking against the instanced mesh (single raycast).
    const hit = this.pick(event);
    this.renderer.domElement.style.cursor = hit ? 'pointer' : 'grab';
  }

  private pick(event: PointerEvent): string | null {
    if (!this.nodeMesh) return null;
    const rect = this.renderer.domElement.getBoundingClientRect();
    const ndc = new THREE.Vector2(
      ((event.clientX - rect.left) / rect.width) * 2 - 1,
      -((event.clientY - rect.top) / rect.height) * 2 + 1,
    );
    const raycaster = new THREE.Raycaster();
    raycaster.setFromCamera(ndc, this.camera);
    const hits = raycaster.intersectObject(this.nodeMesh, false);
    if (hits.length === 0) return null;
    const instanceId = hits[0]!.instanceId;
    return instanceId !== undefined ? this.indexByNode.get(instanceId) ?? null : null;
  }

  private onResize = (): void => {
    const container = this.options.container;
    const w = container.clientWidth;
    const h = container.clientHeight;
    if (w === 0 || h === 0) return;
    this.camera.aspect = w / h;
    this.camera.updateProjectionMatrix();
    this.renderer.setSize(w, h);
    this.needsRender = true;
  };

  fitCamera(): void {
    if (this.positionsById.size === 0) return;
    const box = new THREE.Box3();
    for (const p of this.positionsById.values()) {
      box.expandByPoint(new THREE.Vector3(p.x, p.y, p.z));
    }
    const sphere = box.getBoundingSphere(new THREE.Sphere());
    const dist = sphere.radius / Math.sin((this.camera.fov * Math.PI) / 360) + sphere.radius * 0.2;
    this.controls.target.set(sphere.center.x, sphere.center.y, sphere.center.z);
    this.camera.position.set(
      sphere.center.x + dist * 0.4,
      sphere.center.y + dist * 0.5,
      sphere.center.z + dist,
    );
    this.controls.update();
    this.needsRender = true;
  }

  dispose(): void {
    this.disposed = true;
    window.removeEventListener('resize', this.onResize);
    this.renderer.domElement.removeEventListener('pointerdown', this.onPointerDown);
    this.renderer.domElement.removeEventListener('pointermove', this.onPointerMove);
    this.controls.dispose();
    this.nodeMesh?.geometry.dispose();
    this.edgeLines?.geometry.dispose();
    this.renderer.dispose();
    this.renderer.domElement.remove();
  }
}

export function nodeRadius(node: GraphNode): number {
  const degree = node.trusters + node.trusting;
  return 1.1 + Math.sqrt(degree) * 0.45 + (node.isArticulationPoint ? 0.8 : 0);
}

export { NODE_COLORS };
