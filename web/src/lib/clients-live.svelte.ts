// The connection to the live stream. It is closed while the tab is hidden and opened again when the
// reader comes back. Nothing depends on it: without it the pages keep polling.
import { LIVE_STEP_MS, LIVE_STREAM_URL, type LiveTick } from './clients-live';

/** Ten minutes of ticks, the widest window that shows every one of them. */
const KEEP = Math.ceil(600_000 / LIVE_STEP_MS);

export interface ClientsLive {
  readonly ticks: readonly LiveTick[];
  readonly last: LiveTick | null;
  /** When the last tick came, by the clock of this browser. */
  readonly at: number | null;
  /** The stream is open and sends. */
  readonly connected: boolean;
}

/** `onchanged` runs when the state of the pool changed: the page reads its data again. */
export function useClientsLive(onchanged?: () => void, source?: typeof EventSource | null): ClientsLive {
  let ticks = $state.raw<LiveTick[]>([]);
  let at = $state<number | null>(null);
  let connected = $state(false);

  $effect(() => {
    const Source = source === undefined ? globalThis.EventSource : source;
    if (!Source) return;
    let stream: EventSource | null = null;

    const close = () => {
      stream?.close();
      stream = null;
      connected = false;
    };
    const open = () => {
      if (stream || document.hidden) return;
      const next = new Source(LIVE_STREAM_URL);
      stream = next;
      next.onerror = () => {
        connected = false;
      };
      next.addEventListener('clients', (event) => {
        let tick: LiveTick;
        try {
          tick = JSON.parse((event as MessageEvent).data) as LiveTick;
        } catch {
          return;
        }
        ticks = [...ticks, tick].slice(-KEEP);
        at = Date.now();
        connected = true;
      });
      next.addEventListener('changed', () => onchanged?.());
    };
    const onVisibility = () => (document.hidden ? close() : open());

    document.addEventListener('visibilitychange', onVisibility);
    open();
    return () => {
      document.removeEventListener('visibilitychange', onVisibility);
      close();
    };
  });

  return {
    get ticks() {
      return ticks;
    },
    get last() {
      return ticks[ticks.length - 1] ?? null;
    },
    get at() {
      return at;
    },
    get connected() {
      return connected;
    },
  };
}
