// What every panel page shares: the API client, a fetch that notices an ended session, and a
// counter the live stream bumps to make the pages ask again at once.
import { getContext, setContext } from 'svelte';
import { createClient } from '../lib/api';

export interface Panel {
  fetchFn: typeof fetch;
  client: ReturnType<typeof createClient>;
  /** Bumped when something changed on the server; pages refetch when it moves. */
  readonly tick: number;
  refreshAll(): void;
}

const KEY = Symbol('panel');

export function createPanel(fetchImpl: typeof fetch, onUnauthorized: () => void): Panel {
  let tick = $state(0);
  const fetchFn: typeof fetch = async (input, init) => {
    const response = await fetchImpl(input, init);
    if (response.status === 401) onUnauthorized();
    return response;
  };
  return {
    fetchFn,
    client: createClient(fetchFn, { onUnauthorized }),
    get tick() {
      return tick;
    },
    refreshAll() {
      tick += 1;
    },
  };
}

export const providePanel = (panel: Panel) => setContext(KEY, panel);
export const usePanel = () => getContext<Panel>(KEY);
