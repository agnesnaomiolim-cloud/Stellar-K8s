import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import ForceGraph from './components/force_graph';
import NodePanel from './components/node_panel';
import FpsMeter from './components/fps_meter';
import Hud from './components/hud';
import Legend from './components/legend';
import Toolbar from './components/toolbar';
import { buildTrustGraph } from './topology/graph';
import type { QuorumGraph, GraphNode, FpsStats } from './topology/types';
import { summarizeGraph } from './topology/layout';
import type { GraphSummary } from './topology/layout';
import { QuorumWsClient } from './topology/ws_client';
import type { WsStatus } from './topology/ws_client';

type Source = 'snapshot' | 'live';

const SNAPSHOT_URL = '/snapshots/quorum_snapshot.json';

export default function App() {
  const [graph, setGraph] = useState<QuorumGraph | null>(null);
  const [summary, setSummary] = useState<GraphSummary | null>(null);
  const [selected, setSelected] = useState<GraphNode | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [fps, setFps] = useState<FpsStats | null>(null);
  const [source, setSource] = useState<Source>('snapshot');
  const [liveStatus, setLiveStatus] = useState<WsStatus>('idle');

  const rendererRef = useRef<{ fitCamera(): void; setSelected(id: string | null): void } | null>(
    null,
  );
  const wsRef = useRef<QuorumWsClient | null>(null);

  const fpsBufferRef = useRef<number[]>([]);

  const handleFps = useCallback((value: number, frameMs: number) => {
    fpsBufferRef.current.push(value);
    if (fpsBufferRef.current.length > 60) fpsBufferRef.current.shift();
    setFps({ fps: value, frameMs, samples: fpsBufferRef.current.length });
  }, []);

  const handleNodeSelect = useCallback((node: GraphNode | null) => {
    setSelected(node);
  }, []);

  const ingest = useCallback((raw: unknown) => {
    try {
      const built = buildTrustGraph(raw);
      setGraph(built);
      setSummary(summarizeGraph(built));
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  // Initial snapshot load
  const loadSnapshot = useCallback(async () => {
    setLoading(true);
    try {
      const res = await fetch(SNAPSHOT_URL);
      if (!res.ok) throw new Error(`snapshot fetch failed: HTTP ${res.status}`);
      const raw = (await res.json()) as unknown;
      ingest(raw);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [ingest]);

  useEffect(() => {
    void loadSnapshot();
  }, [loadSnapshot]);

  // Live WebSocket source
  useEffect(() => {
    if (source !== 'live') {
      wsRef.current?.close();
      wsRef.current = null;
      setLiveStatus('idle');
      return;
    }
    const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
    const url = `${proto}://${window.location.host}/ws/quorum`;
    const client = new QuorumWsClient({
      url,
      onUpdate: (dump) => ingest(dump),
      onStatus: setLiveStatus,
    });
    wsRef.current = client;
    client.connect();
    return () => {
      client.close();
    };
  }, [source, ingest]);

  const handleFit = useCallback(() => {
    rendererRef.current?.fitCamera();
  }, []);

  const handleReload = useCallback(() => {
    if (source === 'snapshot') void loadSnapshot();
  }, [source, loadSnapshot]);

  const overlay = useMemo(() => {
    if (loading) {
      return (
        <div className="topology-overlay">
          <div className="topology-overlay__ring" />
          <div className="topology-overlay__title">Loading quorum topology…</div>
          <div className="topology-overlay__sub">parsing stellar-core dump &amp; building trust graph</div>
        </div>
      );
    }
    if (error) {
      return (
        <div className="topology-overlay">
          <div className="topology-overlay__title">Failed to load topology</div>
          <div className="topology-overlay__sub">{error}</div>
          <button className="topology-toolbar__btn" onClick={() => void loadSnapshot()}>
            Retry
          </button>
        </div>
      );
    }
    return null;
  }, [loading, error, loadSnapshot]);

  return (
    <>
      <ForceGraph
        graph={graph}
        onNodeSelect={handleNodeSelect}
        onFps={handleFps}
      />
      <Hud summary={summary} />
      <Toolbar
        source={source}
        onSourceChange={setSource}
        onFit={handleFit}
        onReload={handleReload}
        liveStatus={liveStatus}
      />
      <Legend />
      <FpsMeter stats={fps} />
      <NodePanel node={selected} onClose={() => setSelected(null)} />
      {overlay}
    </>
  );
}
