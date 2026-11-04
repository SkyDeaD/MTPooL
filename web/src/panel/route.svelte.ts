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

/** Follows the address bar; returns the function that stops it. */
export function startRouter(): () => void {
  const sync = () => {
    current.route = parseRoute(window.location.hash);
  };
  sync();
  window.addEventListener('hashchange', sync);
  return () => window.removeEventListener('hashchange', sync);
}

export function go(page: Page, arg?: string | null): void {
  window.location.hash = hrefOf(page, arg);
}
