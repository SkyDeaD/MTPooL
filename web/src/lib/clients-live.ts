// The live numbers of the pool: what `GET /api/v1/stream` sends between two polls. Pure functions;
// `clients-live.svelte.ts` keeps the connection.
import type { ClientsSeries, LiveClients, PublicDomain, PublicNode } from './types';

export const LIVE_STREAM_URL = '/api/v1/stream';
/** How often the registrar sends the numbers. */
export const LIVE_STEP_MS = 5000;

export type LiveTick = LiveClients;

/** The series with what came after its last point. A long range gets only the newest moment. */
export function mergeLive(series: ClientsSeries, ticks: readonly LiveTick[]): ClientsSeries {
  const last = series.points[series.points.length - 1];
  const after = last ? Date.parse(last.at) : -Infinity;
  const fresh = ticks.filter((tick) => Date.parse(tick.at) > after);
  if (fresh.length === 0) return series;
  const shown = series.range === '10m' || series.range === '1h' ? fresh : fresh.slice(-1);
  return {
    ...series,
    points: [...series.points, ...shown.map(({ at, total, domains }) => ({ at, total, domains }))],
  };
}

/** The tables and the scheme with the numbers of the last tick. */
export function withLiveClients<N extends PublicNode>(
  nodes: N[],
  domains: PublicDomain[],
  tick: LiveTick | null,
): { nodes: N[]; domains: PublicDomain[] } {
  if (!tick) return { nodes, domains };
  return {
    nodes: nodes.map((n) =>
      n.id in tick.nodes ? { ...n, clients: tick.nodes[n.id], other_clients: tick.other_nodes?.[n.id] } : n,
    ),
    domains: domains.map((d) => (d.domain in tick.domains ? { ...d, clients: tick.domains[d.domain] } : d)),
  };
}
