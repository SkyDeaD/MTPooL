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

export function useResource<T>(url: () => string, fetchFn?: typeof fetch): Resource<T> {
  let data = $state.raw<T | null>(null);
  let stale = $state(false);
  let settled = $state(false);
  let updatedAt = $state<number | null>(null);
  let status = $state<number | null>(null);

  $effect(() => {
    const poller = createPoller<T>({
      url: url(),
      fetchFn,
      onData(next) {
        data = next;
        status = null;
        stale = false;
        settled = true;
        updatedAt = Date.now();
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
