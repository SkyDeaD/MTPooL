// A value kept fresh by polling an admin path every 30 seconds (with ETag), and again at once
// when the live stream says something changed. Call it while a component is being created.
import { createPoller, PollError } from '../lib/poll';
import { usePanel } from './context.svelte';

export interface Polled<T> {
  readonly data: T | null;
  /** The latest request failed: what is shown may be old. */
  readonly stale: boolean;
  readonly settled: boolean;
  /** HTTP status of the latest failed request. */
  readonly status: number | null;
  refresh(): void;
}

export function usePolled<T>(url: () => string | null): Polled<T> {
  const panel = usePanel();
  let data = $state.raw<T | null>(null);
  let stale = $state(false);
  let settled = $state(false);
  let status = $state<number | null>(null);
  let again = $state(0);

  $effect(() => {
    void panel.tick;
    void again;
    const target = url();
    if (target === null) return;
    const poller = createPoller<T>({
      url: target,
      fetchFn: panel.fetchFn,
      onData(next) {
        data = next;
        status = null;
        stale = false;
        settled = true;
      },
      onUnchanged() {
        stale = false;
      },
      onError(error) {
        stale = true;
        settled = true;
        status = error instanceof PollError ? error.status : null;
      },
    });
    poller.start();
    return () => poller.stop();
  });

  return {
    get data() {
      return data;
    },
    get stale() {
      return stale;
    },
    get settled() {
      return settled;
    },
    get status() {
      return status;
    },
    refresh() {
      again += 1;
    },
  };
}
