// A value kept fresh by polling an API path. Call it while a component is being created.
//
// `data` is the last good answer and stays when a later request fails; `stale` says that the
// latest request did fail, so the page can tell the reader the numbers are old.
import { createPoller, PollError } from './poll';

export interface Resource<T> {
  readonly data: T | null;
  /** The last request failed (the data, if any, is older than it looks). */
  readonly stale: boolean;
  /** An answer or a failure has come at least once. */
  readonly settled: boolean;
  /** When the data was last confirmed current, in ms. */
  readonly updatedAt: number | null;
  /** The HTTP status of the last failed request; null after an answer or a network failure. */
  readonly status: number | null;
}

// Each public tab is a page of its own, so without this it starts empty every time. The last answers
// live for the session and are shown until the fresh one comes. Public data only: the panel has its
// own client and keeps nothing here.
const SEEN = 'mtpool-seen';

function recall<T>(url: string): { data: T; at: number } | null {
  try {
    const saved = sessionStorage.getItem(`mtpool-answer:${url}`);
    return saved ? (JSON.parse(saved) as { data: T; at: number }) : null;
  } catch {
    return null;
  }
}

function remember(url: string, data: unknown, at: number): void {
  try {
    sessionStorage.setItem(`mtpool-answer:${url}`, JSON.stringify({ data, at }));
    sessionStorage.setItem(SEEN, '1');
  } catch {
    // Storage is blocked or full: the page simply waits for its answers.
  }
}

export function useResource<T>(url: () => string, fetchFn?: typeof fetch): Resource<T> {
  let data = $state.raw<T | null>(null);
  let stale = $state(false);
  let settled = $state(false);
  let updatedAt = $state<number | null>(null);
  let status = $state<number | null>(null);

  $effect(() => {
    const path = url();
    const known = recall<T>(path);
    if (known) {
      data = known.data;
      settled = true;
      updatedAt = known.at;
    }
    const poller = createPoller<T>({
      url: path,
      fetchFn,
      onData(next) {
        data = next;
        status = null;
        stale = false;
        settled = true;
        updatedAt = Date.now();
        remember(path, next, updatedAt);
      },
      onUnchanged() {
        stale = false;
        updatedAt = Date.now();
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
    get updatedAt() {
      return updatedAt;
    },
    get status() {
      return status;
    },
  };
}
