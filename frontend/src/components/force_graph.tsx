import { useEffect, useRef } from 'react';
import type { QuorumGraph, GraphNode } from '../topology/types';
import { TopologyRenderer } from '../topology/webgl_renderer';
import type { LayoutRequest, LayoutResponse, LayoutTickPayload } from '../topology/layout';

export interface ForceGraphProps {
  graph: QuorumGraph | null;
  /** Called when the user clicks a node (null = deselected). */
  onNodeSelect: (node: GraphNode | null) => void;
  /** FPS sampling from the render loop. */
  onFps: (fps: number, frameMs: number) => void;
}

/**
 * Mounts the imperative `TopologyRenderer` into a div and bridges the
 * physics worker with the renderer. React never re-renders the canvas —
 * all per-frame work stays outside the vDOM for predictable 60 FPS.
 */
export default function ForceGraph({ graph, onNodeSelect, onFps }: ForceGraphProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const rendererRef = useRef<TopologyRenderer | null>(null);
  const workerRef = useRef<Worker | null>(null);
  const lastTickPayloadRef = useRef<LayoutTickPayload | null>(null);

  // Renderer lifecycle (once)
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const renderer = new TopologyRenderer({
      container,
      onNodeSelect,
      onFps,
    });
    rendererRef.current = renderer;
    return () => {
      renderer.dispose();
      rendererRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Worker lifecycle (once)
  useEffect(() => {
    const worker = new Worker(new URL('../topology/layout.worker.ts', import.meta.url), {
      type: 'module',
    });
    workerRef.current = worker;
    return () => {
      worker.terminate();
      workerRef.current = null;
    };
  }, []);

  // Graph data → renderer + worker
  useEffect(() => {
    const renderer = rendererRef.current;
    const worker = workerRef.current;
    if (!graph || !renderer || !worker) return;

    renderer.setGraph(graph);

    const request: LayoutRequest = {
      type: 'layout:init',
      graph: {
        nodes: graph.nodes.map((n) => ({
          id: n.id,
          trusters: n.trusters,
          trusting: n.trusting,
        })),
        edges: graph.edges.map((e) => ({ source: e.source, target: e.target, kind: e.kind })),
      },
    };
    worker.postMessage(request);
    const onMessage = (event: MessageEvent<LayoutResponse>) => {
      if (event.data?.type !== 'layout:tick') return;
      lastTickPayloadRef.current = event.data.payload;
      renderer.applyTick(event.data.payload);
    };
    worker.addEventListener('message', onMessage);
    return () => {
      worker.removeEventListener('message', onMessage);
    };
  }, [graph, rendererRef, workerRef]);

  return <div ref={containerRef} style={{ position: 'absolute', inset: 0 }} />;
}
