// The live stream of the registrar (`GET /api/v1/admin/stream`, server-sent events).
//
// The events are named, so each kind gets its own listener. The stream has no replay: after a
// break the panel cannot know what it missed, so a reconnect asks `onResync` to read everything
// again. While the stream is down the pages keep polling every 30 seconds, so nothing depends on
// it. The session cookie goes along by itself, the stream is on the same origin.

export type StreamState = 'connecting' | 'live' | 'down' | 'unsupported';

export const STREAM_KINDS = [
  'node_registered',
  'node_ip_changed',
  'node_quarantined',
  'node_recovered',
  'node_banned',
  'node_reverify_failed',
  'node_dead',
  'node_pruned',
  'ban_lifted',
  'master_changed',
  'sensor_round',
  'dns_applied',
  'dns_failed',
  'srmd_action',
  'clock_skew',
  'store_error',
  'rollout_changed',
] as const;

export const STREAM_URL = '/api/v1/admin/stream';
const CLOSED = 2;

export interface StreamOptions {
  /** The EventSource class; null where there is none. */
  source: typeof EventSource | null;
  onState(state: StreamState): void;
  /** One event came: its kind and the parsed body (null when it was not JSON). */
  onEvent(kind: string, data: unknown): void;
  /** The connection is back after a break: read the data again. */
  onResync(): void;
  /** Pause before a new connection after the browser gave up on the old one. */
  retryMs?: number;
}

/** Opens the stream; the returned function closes it for good. */
export function connectStream(options: StreamOptions): () => void {
  const { source: Source, onState, onEvent, onResync, retryMs = 5000 } = options;
  if (!Source) {
    onState('unsupported');
    return () => {};
  }
  let current: EventSource | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopped = false;
  let wasDown = false;

  function open(): void {
    onState('connecting');
    const es = new Source!(STREAM_URL);
    current = es;
    for (const kind of STREAM_KINDS) {
      es.addEventListener(kind, (event) => {
        let data: unknown = null;
        try {
          data = JSON.parse((event as MessageEvent).data);
        } catch {
          // A frame that is not JSON still tells that something happened.
        }
        onEvent(kind, data);
      });
    }
    es.onopen = () => {
      onState('live');
      if (wasDown) onResync();
      wasDown = false;
    };
    es.onerror = () => {
      if (stopped) return;
      wasDown = true;
      onState('down');
      // While the browser still retries by itself, leave it be. A closed connection is ours to reopen.
      if (es.readyState === CLOSED) {
        es.close();
        timer = setTimeout(open, retryMs);
      }
    };
  }

  open();
  return () => {
    stopped = true;
    clearTimeout(timer);
    current?.close();
  };
}
