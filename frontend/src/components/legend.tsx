/** Static color legend for edges and node classes. */
export default function Legend() {
  return (
    <div className="topology-legend" aria-hidden>
      <div className="topology-legend__title">Trust edges</div>
      <div className="topology-legend__item">
        <span className="topology-legend__swatch" style={{ background: 'var(--direct)' }} />
        direct trust
      </div>
      <div className="topology-legend__item">
        <span className="topology-legend__swatch" style={{ background: 'var(--indirect)' }} />
        indirect (transitive) trust
      </div>
      <div className="topology-legend__item">
        <span className="topology-legend__swatch" style={{ background: 'var(--missing)' }} />
        missing / unpublished peer
      </div>
      <div className="topology-legend__title" style={{ marginTop: 6 }}>
        Nodes
      </div>
      <div className="topology-legend__item">
        <span className="topology-legend__swatch topology-legend__swatch--sphere" style={{ background: '#8fb7e8' }} />
        validator
      </div>
      <div className="topology-legend__item">
        <span className="topology-legend__swatch topology-legend__swatch--sphere" style={{ background: 'var(--warning)' }} />
        hub (degree ≥ 12)
      </div>
      <div className="topology-legend__item">
        <span className="topology-legend__swatch topology-legend__swatch--sphere" style={{ background: 'var(--missing)' }} />
        single-point-of-failure
      </div>
    </div>
  );
}
