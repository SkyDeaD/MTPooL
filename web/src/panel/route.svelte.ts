// The panel's address is the part after `#/`: `#/nodes/alpha-k3m2q`. Using the fragment keeps
// the route through a login and needs nothing from the server beyond /panel itself.

export type Page = 'overview' | 'nodes' | 'node' | 'masters' | 'srmd' | 'journal' | 'settings' | 'owners' | 'updates';

export interface Route {
  page: Page;
  /** The node id on a node page, the tab on the journal. */
  arg: string | null;
}

const PAGES: readonly string[] = ['overview', 'nodes', 'masters', 'srmd', 'journal', 'settings', 'owners', 'updates'];

export function parseRoute(hash: string): Route {
  const parts = hash.replace(/^#\/?/, '').split('/').filter(Boolean).map(decodeURIComponent);
  const [head, arg] = parts;
  if (head === 'nodes' && arg) return { page: 'node', arg };
  if (head && PAGES.includes(head)) return { page: head as Page, arg: arg ?? null };
  return { page: 'overview', arg: null };
}

export function hrefOf(page: Page, arg?: string | null): string {
  const base = page === 'node' ? 'nodes' : page;
  return `#/${base}${arg ? `/${encodeURIComponent(arg)}` : ''}`;
}

export const current = $state<{ route: Route }>({ route: { page: 'overview', arg: null } });

/**
 * A page with unsaved work sets `dirty`. A move to another address is then held back: the address
 * goes back where it was and `pending` keeps the one asked for, until the shell has asked the reader.
 */
export const guard = $state<{ dirty: boolean; pending: string | null }>({ dirty: false, pending: null });

/** The reader agreed to leave: the held address is taken. */
export function leave(): void {
  const target = guard.pending;
  guard.dirty = false;
  guard.pending = null;
  if (target !== null) window.location.hash = target;
}

/** Follows the address bar; returns the function that stops it. */
export function startRouter(): () => void {
  let last = window.location.hash;
  const sync = () => {
    const next = window.location.hash;
    if (guard.dirty && next !== last) {
      guard.pending = next;
      history.replaceState(null, '', last || '#/');
      return;
    }
    last = next;
    current.route = parseRoute(next);
  };
  sync();
  window.addEventListener('hashchange', sync);
  return () => {
    window.removeEventListener('hashchange', sync);
    guard.dirty = false;
    guard.pending = null;
  };
}

export function go(page: Page, arg?: string | null): void {
  window.location.hash = hrefOf(page, arg);
}
