// The connection to the live stream. It is closed while the tab is hidden and opened again when the
// reader comes back. Nothing depends on it: without it the pages keep polling.
import { LIVE_STEP_MS, LIVE_STREAM_URL, type LiveTick } from './clients-live';

/** Ten minutes of ticks, the widest window that shows every one of them. */
const KEEP = Math.ceil(600_000 / LIVE_STEP_MS);
/** How long after a refused or broken connection the page asks again: the browser will not. */
const REOPEN_MS = 30_000;
/** Everyone reads the lists again within this time after a change, not all in the same instant. */
const SPREAD_MS = 2000;

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
    let reopening: ReturnType<typeof setTimeout> | undefined;
    let reading: ReturnType<typeof setTimeout> | undefined;

    const close = () => {
      clearTimeout(reopening);
      clearTimeout(reading);
      reading = undefined;
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
        // A plain break reconnects by itself. An answer other than the stream ends it for good.
        if (next.readyState === Source.CLOSED) {
          stream = null;
          clearTimeout(reopening);
          reopening = setTimeout(open, REOPEN_MS + Math.random() * REOPEN_MS);
        }
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
      next.addEventListener('changed', () => {
        if (!onchanged || reading !== undefined) return;
        reading = setTimeout(() => {
          reading = undefined;
          onchanged();
        }, Math.random() * SPREAD_MS);
      });
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
