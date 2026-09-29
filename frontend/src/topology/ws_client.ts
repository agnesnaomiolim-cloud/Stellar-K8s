import type { QuorumDump } from './types.js'

export type WsStatus = 'idle' | 'connecting' | 'open' | 'error';

export interface QuorumWsClientOptions {
  url: string;
  /** Called with each parsed quorum dump pushed by the server. */
  onUpdate: (dump: QuorumDump) => void;
  onStatus: (status: WsStatus) => void;
  /** Reconnect backoff base in ms. Default 1000. */
  backoffMs?: number;
  /** Maximum backoff in ms. Default 15000. */
  maxBackoffMs?: number;
}

/**
 * Resilient WebSocket client for real-time quorum topology streaming.
 *
 * Expected server messages:
 *  - `{ type: 'quorum', dump: <QuorumDump> }` — full snapshot push
 *  - `{ type: 'quorum:update', ...dump }`     — delta push
 *
 * Automatically reconnects with exponential backoff + jitter, and
 * tolerates server restarts without leaking handlers.
 */
export class QuorumWsClient {
  private ws: WebSocket | null = null;
  private attempts = 0;
  private closed = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(private options: QuorumWsClientOptions) {}

  connect(): void {
    if (this.closed) return;
    this.options.onStatus('connecting');
    try {
      this.ws = new WebSocket(this.options.url);
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.ws.onopen = () => {
      this.attempts = 0;
      this.options.onStatus('open');
    };
    this.ws.onmessage = (event: MessageEvent) => {
      try {
        const parsed = JSON.parse(String(event.data)) as {
          type?: string;
          dump?: QuorumDump;
        } & QuorumDump;
        if (parsed.type === 'quorum' && parsed.dump) {
          this.options.onUpdate(parsed.dump);
        } else if (parsed.type === 'quorum:update') {
          this.options.onUpdate(parsed as QuorumDump);
        }
      } catch {
        // Ignore malformed frames; never let one bad message kill the pipe.
      }
    };
    this.ws.onerror = () => {
      this.options.onStatus('error');
    };
    this.ws.onclose = () => {
      if (!this.closed) this.scheduleReconnect();
    };
  }

  private scheduleReconnect(): void {
    if (this.closed || this.reconnectTimer) return;
    const base = this.options.backoffMs ?? 1000;
    const max = this.options.maxBackoffMs ?? 15000;
    const jitter = Math.random() * 0.3 * base;
    const delay = Math.min(base * 2 ** this.attempts + jitter, max);
    this.attempts += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }

  close(): void {
    this.closed = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.onopen = null;
      this.ws.close();
      this.ws = null;
    }
    this.options.onStatus('idle');
  }
}
