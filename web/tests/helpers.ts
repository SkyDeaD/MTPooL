// Shared by the component tests: a fake fetch over the fixtures, with ETags like the registrar's,
// and a way to let timers and promises run.
import { flushSync } from 'svelte';
import { vi } from 'vitest';
import bans from './fixtures/bans.json';
import domains from './fixtures/domains.json';
import links from './fixtures/links.json';
import nodeHistory from './fixtures/node-history.json';
import nodes from './fixtures/nodes.json';
import clients10m from './fixtures/stats-clients-10m.json';
import clients1h from './fixtures/stats-clients-1h.json';
import clients24h from './fixtures/stats-clients-24h.json';
import clients7d from './fixtures/stats-clients-7d.json';
import series24h from './fixtures/stats-series-24h.json';
import series30d from './fixtures/stats-series-30d.json';
import series7d from './fixtures/stats-series-7d.json';
import summary24h from './fixtures/stats-summary-24h.json';
import summary30d from './fixtures/stats-summary-30d.json';
import summary7d from './fixtures/stats-summary-7d.json';

/** The instant the fixtures were made at. Tests fix the clock here. */
export const FIXTURE_NOW = Date.parse('2026-11-04T12:00:00Z');

type Json = unknown;

const byRange: Record<string, Record<string, Json>> = {
  'stats/summary': { '24h': summary24h, '7d': summary7d, '30d': summary30d },
  'stats/series': { '24h': series24h, '7d': series7d, '30d': series30d },
  'stats/clients': { '10m': clients10m, '1h': clients1h, '24h': clients24h, '7d': clients7d },
};

export interface FakeApi {
  fetchFn: typeof fetch;
  /** Every request made, as `{url, etag}` (`etag` is the If-None-Match sent). */
  calls: { url: string; etag: string | null }[];
  /** Replaces the body of a path (`nodes`, `domains`, ...). */
  set(path: string, body: Json): void;
  /** Makes every request fail with a network error until `fail(false)`. */
  fail(on: boolean): void;
}

export function fakeApi(): FakeApi {
  const bodies: Record<string, Json> = {
    nodes,
    domains,
    bans,
    links,
  };
  let failing = false;
  const calls: FakeApi['calls'] = [];
  const fetchFn = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), 'http://localhost');
    const etag = new Headers(init?.headers).get('If-None-Match');
    calls.push({ url: url.pathname + url.search, etag });
    if (failing) throw new TypeError('network down');
    const path = url.pathname.replace(/^\/api\/v1\//, '');
    let body: Json;
    const history = path.match(/^nodes\/([^/]+)\/history$/);
    const one = path.match(/^nodes\/([^/]+)$/);
    if (byRange[path]) body = byRange[path][url.searchParams.get('range') ?? ''];
    else if (history) body = nodeHistory;
    else if (one) body = (bodies.nodes as { id: string }[]).find((n) => n.id === one[1]);
    else body = bodies[path];
    if (body === undefined) return new Response('{"code":"not_found"}', { status: 404 });
    const text = JSON.stringify(body);
    const tag = `"${path}-${text.length}"`;
    if (etag === tag) return new Response(null, { status: 304, headers: { ETag: tag } });
    return new Response(text, { status: 200, headers: { ETag: tag, 'Content-Type': 'application/json' } });
  }) as unknown as typeof fetch;
  return {
    fetchFn,
    calls,
    set: (path, body) => {
      bodies[path] = body;
    },
    fail: (on) => {
      failing = on;
    },
  };
}

/** Lets pending promises and a little fake time pass, then applies Svelte's updates. */
export async function settle(ms = 5): Promise<void> {
  for (let i = 0; i < 4; i++) {
    await vi.advanceTimersByTimeAsync(ms);
    flushSync();
  }
}

/** Pretends the tab is hidden or shown. */
export function setHidden(hidden: boolean): void {
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  document.dispatchEvent(new Event('visibilitychange'));
}
