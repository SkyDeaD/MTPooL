// Asks one URL again and again: every 30 seconds, with the ETag of the last answer, and not at
// all while the tab is hidden. Coming back to the tab asks at once.

export const POLL_INTERVAL_MS = 30_000;

export interface PollerOptions<T> {
  url: string;
  intervalMs?: number;
  fetchFn?: typeof fetch;
  /** A new body arrived (status 200). */
  onData(data: T): void;
  /** The server said 304: what is shown is still current. */
  onUnchanged?(): void;
  /** The request failed or the answer was not a success. The last data stays with the caller. */
  onError(error: unknown): void;
}

export interface Poller {
  start(): void;
  stop(): void;
  /** Asks now, without waiting for the interval. */
  refresh(): void;
}

export class PollError extends Error {
  constructor(readonly status: number) {
    super(`HTTP ${status}`);
    this.name = 'PollError';
  }
}

export function createPoller<T>(options: PollerOptions<T>): Poller {
  const { url, intervalMs = POLL_INTERVAL_MS, onData, onUnchanged, onError } = options;
  let timer: ReturnType<typeof setInterval> | undefined;
  let etag: string | null = null;
  let running = false;
  let busy = false;
  let stopped = true;

  async function tick(): Promise<void> {
    if (busy) return;
    busy = true;
    try {
      const doFetch = options.fetchFn ?? globalThis.fetch.bind(globalThis);
      const res = await doFetch(url, etag ? { headers: { 'If-None-Match': etag } } : undefined);
      if (stopped) return;
      if (res.status === 304) {
        onUnchanged?.();
      } else if (!res.ok) {
        throw new PollError(res.status);
      } else {
        const data = (await res.json()) as T;
        if (stopped) return;
        etag = res.headers.get('ETag');
        onData(data);
      }
    } catch (error) {
      if (!stopped) onError(error);
    } finally {
      busy = false;
    }
  }

  function arm(): void {
    clearInterval(timer);
    timer = setInterval(() => {
      if (!document.hidden) void tick();
    }, intervalMs);
  }

  function onVisibility(): void {
    if (document.hidden) return;
    arm();
    void tick();
  }

  return {
    refresh() {
      if (running && !document.hidden) void tick();
    },
    start() {
      if (running) return;
      running = true;
      stopped = false;
      document.addEventListener('visibilitychange', onVisibility);
      arm();
      if (!document.hidden) void tick();
    },
    stop() {
      running = false;
      stopped = true;
      clearInterval(timer);
      document.removeEventListener('visibilitychange', onVisibility);
    },
  };
}
