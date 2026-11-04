// For the panel tests: a fake registrar (the admin mock plus the public fixtures) behind a fetch,
// and an EventSource that the test drives by hand.
import { vi } from 'vitest';
import { createAdminMock, type AdminMock } from '../scripts/admin-mock';
import { fakeApi, type FakeApi } from './helpers';

export interface FakeCall {
  method: string;
  path: string;
  body?: any;
  headers: Record<string, string>;
}

export interface FakeAdmin {
  fetchFn: typeof fetch;
  mock: AdminMock;
  pub: FakeApi;
  /** Every request, in order. */
  calls: FakeCall[];
  /** How many GETs of a path (after /api/v1) were made. */
  count(path: string): number;
  /** The requests that changed something, as `METHOD path`. */
  writes(): string[];
  /** The session ends on the server: the next request gets 401. */
  expire(): void;
}

export function fakeAdmin(): FakeAdmin {
  const mock = createAdminMock();
  const pub = fakeApi();
  const calls: FakeCall[] = [];
  const fetchFn = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), 'http://localhost');
    const path = url.pathname.replace(/^\/api\/v1/, '');
    const method = (init?.method ?? 'GET').toUpperCase();
    const headers: Record<string, string> = {};
    new Headers(init?.headers).forEach((value, key) => {
      headers[key] = value;
    });
    const body = typeof init?.body === 'string' ? JSON.parse(init.body) : undefined;
    calls.push({ method, path: path + url.search, body, headers });
    if (!path.startsWith('/admin/')) return pub.fetchFn(input, init);
    const answer = mock.handle({ method, path, query: url.searchParams, headers, body });
    if (answer.status === 204 || answer.body === undefined) return new Response(null, { status: answer.status });
    const text = JSON.stringify(answer.body);
    let h = 5381;
    for (let i = 0; i < text.length; i++) h = ((h * 33) ^ text.charCodeAt(i)) >>> 0;
    const tag = `"${path}-${h}"`;
    if (method === 'GET' && answer.status === 200 && headers['if-none-match'] === tag) {
      return new Response(null, { status: 304, headers: { ETag: tag } });
    }
    return new Response(text, { status: answer.status, headers: { ETag: tag, 'Content-Type': 'application/json' } });
  }) as unknown as typeof fetch;
  return {
    fetchFn,
    mock,
    pub,
    calls,
    count: (path) => calls.filter((c) => c.method === 'GET' && c.path === path).length,
    writes: () => calls.filter((c) => c.method !== 'GET').map((c) => `${c.method} ${c.path}`),
    expire: () => mock.expire(),
  };
}

/** An EventSource the test drives: `open()`, `emit()` and `fail()` stand for the network. */
export class FakeEventSource {
  static all: FakeEventSource[] = [];
  readyState = 0;
  closed = false;
  private listeners = new Map<string, ((e: MessageEvent) => void)[]>();
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(readonly url: string) {
    FakeEventSource.all.push(this);
  }
  addEventListener(kind: string, fn: (e: MessageEvent) => void) {
    this.listeners.set(kind, [...(this.listeners.get(kind) ?? []), fn]);
  }
  removeEventListener() {}
  close() {
    this.closed = true;
    this.readyState = 2;
  }
  open() {
    this.readyState = 1;
    this.onopen?.();
  }
  emit(kind: string, data: unknown) {
    for (const fn of this.listeners.get(kind) ?? []) fn({ data: JSON.stringify(data) } as MessageEvent);
  }
  /** The connection drops; the browser would try again by itself. */
  fail(closed = false) {
    this.readyState = closed ? 2 : 0;
    this.onerror?.();
  }
  static reset() {
    FakeEventSource.all = [];
  }
  static get last(): FakeEventSource | undefined {
    return FakeEventSource.all[FakeEventSource.all.length - 1];
  }
}
