/** Toolbar with data-source selector and realtime status. */
export interface ToolbarProps {
  source: 'snapshot' | 'live';
  onSourceChange: (source: 'snapshot' | 'live') => void;
  onFit: () => void;
  onReload: () => void;
  liveStatus: 'idle' | 'connecting' | 'open' | 'error';
}

const STATUS_LABEL: Record<ToolbarProps['liveStatus'], string> = {
  idle: 'idle',
  connecting: 'connecting…',
  open: 'live',
  error: 'offline',
};

export default function Toolbar({
  source,
  onSourceChange,
  onFit,
  onReload,
  liveStatus,
}: ToolbarProps) {
  return (
    <div className="topology-toolbar">
      <button
        className={`topology-toolbar__btn ${source === 'snapshot' ? 'is-active' : ''}`}
        onClick={() => onSourceChange('snapshot')}
      >
        Snapshot
      </button>
      <button
        className={`topology-toolbar__btn ${source === 'live' ? 'is-active' : ''}`}
        onClick={() => onSourceChange('live')}
      >
        Live (WebSocket)
      </button>
      <span className="topology-toolbar__sep" />
      <button className="topology-toolbar__btn" onClick={onFit}>
        Fit view
      </button>
      <button className="topology-toolbar__btn" onClick={onReload}>
        Reload
      </button>
      <span className="topology-toolbar__sep" />
      <span
        className={`topology-toolbar__status ${liveStatus === 'open' ? 'is-live' : ''}`}
      >
        ● {STATUS_LABEL[liveStatus]}
      </span>
    </div>
  );
}
