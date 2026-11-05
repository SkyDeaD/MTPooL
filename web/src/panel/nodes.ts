// How the panel sorts the nodes it shows. A banned node has no record any more: it is the business
// of the ban history. A quarantine stays visible, split as the public table splits it.
import type { AdminNode } from './types';

export interface PanelGroups {
  masters: AdminNode[];
  queue: AdminNode[];
  /** Suspected of a block: the sensors check the address. */
  quarantine: AdminNode[];
  /** In quarantine for a port, metrics or a stale report: only out of the queue. */
  outside: AdminNode[];
  /** Pending, dead and pruned: the pool does not count on them. */
  idle: AdminNode[];
}

const IDLE_ORDER: Record<string, number> = { pending: 0, dead: 1, pruned: 2 };

export function panelGroups(nodes: readonly AdminNode[]): PanelGroups {
  const g: PanelGroups = { masters: [], queue: [], quarantine: [], outside: [], idle: [] };
  for (const node of nodes) {
    if (node.state === 'banned') continue;
    if (node.state === 'quarantine') (node.suspected ? g.quarantine : g.outside).push(node);
    else if (node.state === 'healthy') (node.is_master ? g.masters : g.queue).push(node);
    else g.idle.push(node);
  }
  const place = (n: AdminNode) => n.queue_position ?? Number.MAX_SAFE_INTEGER;
  g.queue.sort((a, b) => place(a) - place(b));
  g.idle.sort((a, b) => (IDLE_ORDER[a.state] ?? 9) - (IDLE_ORDER[b.state] ?? 9));
  return g;
}
