import { useEffect, useRef } from 'react';
import type { FpsStats } from '../topology/types';

export interface FpsMeterProps {
  stats: FpsStats | null;
}

const BARS = 48;

/**
 * Rolling FPS histogram. The bar container is rendered once by React and
 * then updated imperatively per sample — React never re-renders per frame,
 * which keeps the meter itself off the jank profile it is measuring.
 */
export default function FpsMeter({ stats }: FpsMeterProps) {
  const barsRef = useRef<HTMLDivElement>(null);
  const labelRef = useRef<HTMLSpanElement>(null);
  const valuesRef = useRef<number[]>([]);

  useEffect(() => {
    if (!stats) return;
    const values = valuesRef.current;
    values.push(stats.fps);
    if (values.length > BARS) values.shift();

    const container = barsRef.current;
    if (container && container.childElementCount === 0) {
      for (let i = 0; i < BARS; i++) {
        const bar = document.createElement('span');
        bar.className = 'fps-meter__bar';
        bar.style.height = '1px';
        container.appendChild(bar);
      }
    }
    if (container) {
      const children = container.children;
      for (let i = 0; i < BARS; i++) {
        const bar = children[i] as HTMLElement | undefined;
        const v = values[i];
        if (!bar) continue;
        if (v === undefined) {
          bar.style.height = '1px';
          bar.className = 'fps-meter__bar';
          continue;
        }
        const h = Math.max(1, Math.min(30, (v / 70) * 30));
        bar.style.height = `${h.toFixed(1)}px`;
        bar.className = `fps-meter__bar ${v >= 55 ? '' : v >= 30 ? 'is-warn' : 'is-bad'}`;
      }
    }
    if (labelRef.current) {
      const fps = Math.round(stats.fps);
      const cls = fps >= 55 ? '' : fps >= 30 ? 'is-warn' : 'is-bad';
      labelRef.current.innerHTML = '';
      const b = document.createElement('b');
      b.className = cls;
      b.textContent = String(fps);
      labelRef.current.appendChild(b);
      labelRef.current.appendChild(
        document.createTextNode(` fps · ${stats.frameMs.toFixed(1)} ms`),
      );
    }
  }, [stats]);

  return (
    <div className="fps-meter" aria-label="frames per second">
      <div ref={barsRef} style={{ display: 'flex', alignItems: 'flex-end', gap: 1 }} />
      <span className="fps-meter__label" ref={labelRef}>
        <b>–</b> fps
      </span>
    </div>
  );
}
