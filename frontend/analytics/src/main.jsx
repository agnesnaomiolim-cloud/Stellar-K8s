import { StrictMode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import TopologyScene from './TopologyScene.jsx';
import QuorumMatrixCanvas from '../../components/webgl/QuorumMatrixCanvas.jsx';
import { createStreamState, ingest, materialize, statusForNode } from './graphModel.js';
import { buildQuorumMatrix, emptyMatrix, matrixStats } from '../matrix/quorumMatrixModel.js';
import './styles.css';

const EMPTY_GRAPH = materialize(createStreamState());
const query = new URLSearchParams(window.location.search);
const sourceFromQuery = query.get('source');
const bridgeUrl = query.get('ws') || 'localhost:8787';
const initialSource = sourceFromQuery === 'mock' || sourceFromQuery === 'kafka' ? sourceFromQuery : 'live';

function streamUrl(source) {
  const protocol = window.location.protocol === 'https:' ? 'wss' : 'ws';
  if (source === 'mock' || source === 'kafka') return `${protocol}://${bridgeUrl}`;
  return `${protocol}://${window.location.host}/api/v1/quorum/topology/stream`;
}

function App() {
  const [source, setSource] = useState(initialSource);
  const [graph, setGraph] = useState(EMPTY_GRAPH);
  const [connection, setConnection] = useState('connecting');
  const [selected, setSelected] = useState(null);
  const [paused, setPaused] = useState(false);
  const [lastUpdate, setLastUpdate] = useState(null);
  const [view, setView] = useState('graph');
  const [matrix, setMatrix] = useState(emptyMatrix());
  const [hoverCell, setHoverCell] = useState(null);
  const streamStateRef = useRef(createStreamState());
  const renderFrameRef = useRef(null);

  useEffect(() => {
    streamStateRef.current = createStreamState();
    setGraph(EMPTY_GRAPH);
    setMatrix(emptyMatrix());
    setHoverCell(null);
    setSelected(null);
    setConnection('connecting');
    let socket;
    let disposed = false;
    const publishGraph = () => {
      if (renderFrameRef.current !== null) return;
      renderFrameRef.current = requestAnimationFrame(() => {
        renderFrameRef.current = null;
        if (disposed) return;
        const nextGraph = materialize(streamStateRef.current);
        setGraph(nextGraph);
        setMatrix(buildQuorumMatrix(nextGraph));
        setLastUpdate(new Date());
      });
    };
    try {
      socket = new WebSocket(streamUrl(source));
      socket.onopen = () => setConnection('live');
      socket.onmessage = (event) => {
        if (disposed) return;
        try {
          const payload = JSON.parse(event.data);
          streamStateRef.current = ingest(streamStateRef.current, payload);
          publishGraph();
        } catch {
          setConnection('error');
        }
      };
      socket.onerror = () => setConnection('error');
      socket.onclose = () => { if (!disposed) setConnection('offline'); };
    } catch {
      setConnection('error');
    }
    return () => {
      disposed = true;
      socket?.close();
      if (renderFrameRef.current !== null) {
        cancelAnimationFrame(renderFrameRef.current);
        renderFrameRef.current = null;
      }
    };
  }, [source]);

  const counts = useMemo(() => {
    const values = graph.nodes.map(statusForNode);
    return {
      synced: values.filter((value) => value === 'synced').length,
      degraded: values.filter((value) => value === 'degraded').length,
      falling: values.filter((value) => value === 'falling-behind').length,
    };
  }, [graph.nodes]);

  const matrixSummary = useMemo(() => (matrix.size ? matrixStats(matrix) : null), [matrix]);

  const selectNode = useCallback((node) => setSelected(node), []);
  const sourceLabel = source === 'mock' ? 'Mock Kafka stream' : source === 'kafka' ? 'Kafka WebSocket bridge' : 'Operator WebSocket';

  return (
    <main className="app-shell">
      <header className="topbar">
        <div className="brand-block">
          <span className="eyebrow">STELLAR / OBSERVABILITY</span>
          <h1>Network topology</h1>
          <p>Multi-cluster quorum health.</p>
        </div>
        <div className="toolbar" role="toolbar" aria-label="Topology controls">
          <label className="select-wrap">
            <span>Data source</span>
            <select value={source} onChange={(event) => setSource(event.target.value)}>
              <option value="live">Live operator stream</option>
              <option value="kafka">Kafka WebSocket bridge</option>
              <option value="mock">Mock Kafka stream</option>
            </select>
          </label>
          <div className="view-toggle" role="tablist" aria-label="View mode">
            <button type="button" role="tab" aria-selected={view === 'graph'} className={view === 'graph' ? 'active' : ''} onClick={() => setView('graph')}>3D graph</button>
            <button type="button" role="tab" aria-selected={view === 'matrix'} className={view === 'matrix' ? 'active' : ''} onClick={() => setView('matrix')}>Quorum matrix</button>
          </div>
          <button className="tool-button" type="button" onClick={() => setPaused((value) => !value)}>
            {paused ? 'Resume motion' : 'Pause motion'}
          </button>
        </div>
      </header>

      <section className="metric-strip" aria-label="Network summary">
        <Metric label="Validators" value={graph.nodes.length.toLocaleString()} detail={`${graph.edges.length.toLocaleString()} quorum links`} />
        <Metric label="Synced" value={counts.synced.toLocaleString()} detail="Externalize phase" tone="green" />
        <Metric label="Degraded" value={counts.degraded.toLocaleString()} detail="Prepare or confirm" tone="amber" />
        <Metric label="Falling behind" value={counts.falling.toLocaleString()} detail="Stalled or unknown" tone="red" />
      </section>

      <section className="workspace">
        <div className="graph-panel">
          <div className="panel-heading">
            <div>
              <span className={`status-dot ${connection}`} />
              <strong>{sourceLabel}</strong>
              <span className="muted">{lastUpdate ? `updated ${lastUpdate.toLocaleTimeString()}` : 'waiting for telemetry'}</span>
            </div>
            <span className="muted">Live graph</span>
          </div>
          {view === 'matrix'
            ? <QuorumMatrixCanvas matrix={matrix} onHoverCell={setHoverCell} />
            : <TopologyScene graph={graph} onSelect={selectNode} selectedId={selected?.id} paused={paused} />}
          {view === 'matrix'
            ? (
              <div className="legend" aria-label="Matrix legend">
                <Legend color="green" label="Agreeing" />
                <Legend color="amber" label="Confirming" />
                <Legend color="blue" label="Lagging" />
                <Legend color="red" label="Diverged" />
                <span className="muted">{hoverCell ? `row ${hoverCell.sourceIndex} → col ${hoverCell.targetIndex}` : `${matrixSummary?.cells.toLocaleString() ?? 0} interconnect cells`}</span>
              </div>
            )
            : (
              <div className="legend" aria-label="Node health legend">
                <Legend color="green" label="Synced" />
                <Legend color="amber" label="Degraded" />
                <Legend color="red" label="Falling behind" />
              </div>
            )}
        </div>

        <aside className="inspector" aria-live="polite">
          <span className="eyebrow">{view === 'matrix' ? 'CELL INSPECTOR' : 'NODE INSPECTOR'}</span>
          {view === 'matrix'
            ? <CellInspector cell={hoverCell} matrix={matrix} summary={matrixSummary} />
            : (selected ? <NodeInspector node={selected} /> : <EmptyInspector />)}
        </aside>
      </section>
    </main>
  );
}

function Metric({ label, value, detail, tone }) {
  return <div className="metric"><span className="metric-label">{label}</span><strong className={tone ? `tone-${tone}` : ''}>{value}</strong><span className="muted">{detail}</span></div>;
}

function Legend({ color, label }) {
  return <span className="legend-item"><span className={`legend-swatch ${color}`} />{label}</span>;
}

function EmptyInspector() {
  return <div className="empty-inspector"><div className="empty-icon">+</div><h2>Select a validator</h2><p>Validator metrics will appear here.</p></div>;
}

function NodeInspector({ node }) {
  const status = statusForNode(node);
  return <>
    <div className="node-heading"><span className={`node-status ${status}`}>{status.replace('-', ' ')}</span><h2>{node.name}</h2><code>{node.fullId}</code></div>
    <dl className="detail-list">
      <Detail label="Cluster" value={node.cluster} />
      <Detail label="SCP phase" value={node.phase} />
      <Detail label="Ballot" value={node.ballotCounter.toLocaleString()} />
      <Detail label="TPS" value={node.tps ? node.tps.toFixed(1) : 'No sample'} />
      <Detail label="Ledger time" value={node.ledgerTimeMs ? `${node.ledgerTimeMs.toFixed(2)} ms` : 'No sample'} />
      <Detail label="Quorum threshold" value={node.threshold || 'Not reported'} />
    </dl>
    <div className="inspector-note">{node.critical ? 'Critical quorum member.' : 'No criticality alert reported.'}{node.stalled ? ' Consensus progress is stalled.' : ''}</div>
  </>;
}

function Detail({ label, value }) {
  return <div className="detail-row"><dt>{label}</dt><dd>{value}</dd></div>;
}

function CellInspector({ cell, matrix, summary }) {
  if (!cell) {
    return (
      <div className="empty-inspector">
        <div className="empty-icon">%</div>
        <h2>Hover a matrix cell</h2>
        <p>Validator trust metrics will appear here.</p>
        {summary && (
          <dl className="detail-list">
            <Detail label="Validators" value={matrix.size.toLocaleString()} />
            <Detail label="Interconnect cells" value={summary.cells.toLocaleString()} />
            <Detail label="Avg trust" value={summary.avgTrust.toFixed(3)} />
            <Detail label="Avg latency" value={`${summary.avgLatencyMs.toFixed(2)} ms`} />
          </dl>
        )}
      </div>
    );
  }
  const source = matrix.nodes[cell.sourceIndex];
  const target = matrix.nodes[cell.targetIndex];
  return (
    <>
      <div className="node-heading">
        <span className={`node-status cell-${cell.agreement}`}>{cell.agreement}</span>
        <h2>{source?.name} → {target?.name}</h2>
        <code>{source?.publicKey}</code>
        <code>{target?.publicKey}</code>
      </div>
      <dl className="detail-list">
        <Detail label="Row validator" value={`${source?.name} (${source?.cluster})`} />
        <Detail label="Column validator" value={`${target?.name} (${target?.cluster})`} />
        <Detail label="Trust weight" value={cell.trust.toFixed(3)} />
        <Detail label="Latency delta" value={`${cell.latencyMs.toFixed(2)} ms`} />
        <Detail label="Row phase" value={source?.phase} />
        <Detail label="Column phase" value={target?.phase} />
        <Detail label="Row TPS" value={source?.tps ? source.tps.toFixed(1) : 'No sample'} />
        <Detail label="Column TPS" value={target?.tps ? target.tps.toFixed(1) : 'No sample'} />
      </dl>
      <div className="inspector-note">
        {cell.agreement === 'agreeing' ? 'Both validators report externalize.'
          : cell.agreement === 'diverged' ? 'At least one validator is stalled.'
            : cell.agreement === 'lagging' ? 'One side is behind the other.'
              : cell.agreement === 'confirming' ? 'Both sides are confirming ballots.' : 'Phase not reported.'}
      </div>
    </>
  );
}

createRoot(document.getElementById('root')).render(<StrictMode><App /></StrictMode>);
