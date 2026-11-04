// A Vite plugin for `npm run dev`: answers /api/v1/* from tests/fixtures, so every public page shows
// full data without a registrar. Timestamps in the fixtures are absolute as of one instant; each
// answer moves them by the time passed since, so countdowns and "ago" texts look alive.
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { Plugin } from 'vite';
import { createAdminMock } from './admin-mock.ts';

const web = join(dirname(fileURLToPath(import.meta.url)), '..');
const FIXTURE_NOW = Date.parse('2026-11-04T12:00:00Z');
const BUCKET_MS = 900_000;
const RFC3339 = /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$/;

function load(name: string): unknown {
  return JSON.parse(readFileSync(join(web, 'tests', 'fixtures', `${name}.json`), 'utf8'));
}

function shift(value: unknown, delta: number): unknown {
  if (typeof value === 'string' && RFC3339.test(value)) {
    return new Date(Date.parse(value) + delta).toISOString().replace('.000Z', 'Z');
  }
  if (Array.isArray(value)) return value.map((v) => shift(v, delta));
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, shift(v, delta)]));
  }
  return value;
}

const QR = (text: string) => {
  // Not a real QR code: a fixed pattern, so the layout can be seen. The registrar draws real ones.
  const cells: string[] = [];
  for (let y = 0; y < 21; y++) {
    for (let x = 0; x < 21; x++) {
      const finder = (x < 7 && y < 7) || (x > 13 && y < 7) || (x < 7 && y > 13);
      if (finder ? x % 6 === 0 || y % 6 === 0 || (x % 6 > 1 && x % 6 < 5 && y % 6 > 1 && y % 6 < 5) || x > 13 || y > 13 : (x * 7 + y * 13 + text.length) % 3 === 0) {
        cells.push(`<rect x="${x}" y="${y}" width="1" height="1"/>`);
      }
    }
  }
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="-2 -2 25 25"><rect x="-2" y="-2" width="25" height="25" fill="#fff"/>${cells.join('')}</svg>`;
};

function answer(path: string, query: URLSearchParams): { status: number; body: unknown } {
  const delta = Date.now() - FIXTURE_NOW;
  const nodes = load('nodes') as { id: string }[];
  if (path === '/nodes') return { status: 200, body: shift(nodes, delta) };
  if (path === '/domains') return { status: 200, body: load('domains') };
  if (path === '/bans') return { status: 200, body: shift(load('bans'), delta) };
  if (path === '/links') return { status: 200, body: load('links') };
  for (const kind of ['summary', 'series', 'clients']) {
    if (path === `/stats/${kind}`) {
      const fallback = kind === 'clients' ? '10m' : '24h';
      const range = query.get('range') ?? fallback;
      try {
        return { status: 200, body: shift(load(`stats-${kind}-${range}`), delta) };
      } catch {
        return { status: 400, body: { code: 'bad_request' } };
      }
    }
  }
  const history = path.match(/^\/nodes\/([^/]+)\/history$/);
  if (history) {
    if (!nodes.some((n) => n.id === history[1])) return { status: 404, body: { code: 'not_found' } };
    // Buckets stay on the 15 minute grid.
    return { status: 200, body: shift(load('node-history'), Math.floor(delta / BUCKET_MS) * BUCKET_MS) };
  }
  const one = path.match(/^\/nodes\/([^/]+)$/);
  if (one) {
    const node = nodes.find((n) => n.id === one[1]);
    return node ? { status: 200, body: shift(node, delta) } : { status: 404, body: { code: 'not_found' } };
  }
  return { status: 404, body: { code: 'not_found' } };
}

// The admin API: one mock for the whole dev server, so a login and the changes made in the panel
// last until the server restarts. The token to sign in with is `good-token`.
// `MTPOOL_MOCK=calm npm run dev` starts without a node in quarantine; `/api/v1/admin/_mock?state=calm`
// (or `=quarantine`) switches a running dev server and signs the mock out.
let admin = createAdminMock(process.env.MTPOOL_MOCK === 'calm' ? 'calm' : 'quarantine');

function readBody(req: IncomingMessage): Promise<unknown> {
  return new Promise((resolve) => {
    const chunks: Buffer[] = [];
    req.on('data', (c: Buffer) => chunks.push(c));
    req.on('end', () => {
      try {
        resolve(chunks.length > 0 ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : undefined);
      } catch {
        resolve(undefined);
      }
    });
  });
}

async function handleAdmin(req: IncomingMessage, res: ServerResponse, path: string, query: URLSearchParams): Promise<void> {
  if (path === '/admin/stream') {
    // Only a heartbeat comment: the panel then shows the live mode, events are not made up here.
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', Connection: 'keep-alive' });
    res.write(': open\n\n');
    const beat = setInterval(() => res.write(': beat\n\n'), 15_000);
    req.on('close', () => clearInterval(beat));
    return;
  }
  if (path === '/admin/_mock') {
    admin = createAdminMock(query.get('state') === 'calm' ? 'calm' : 'quarantine');
    res.setHeader('Content-Type', 'application/json');
    res.end(JSON.stringify({ state: query.get('state') === 'calm' ? 'calm' : 'quarantine' }));
    return;
  }
  const headers: Record<string, string> = {};
  for (const [k, v] of Object.entries(req.headers)) headers[k] = Array.isArray(v) ? v.join(',') : (v ?? '');
  const body = await readBody(req);
  const out = admin.handle({ method: req.method ?? 'GET', path, query, headers, body });
  const text = out.body === undefined ? '' : JSON.stringify(shift(out.body, Date.now() - FIXTURE_NOW));
  res.statusCode = out.status;
  if (text) res.setHeader('Content-Type', 'application/json');
  if (out.status === 200 && req.method === 'GET') {
    const tag = `"${createHash('sha1').update(text).digest('hex').slice(0, 16)}"`;
    res.setHeader('ETag', tag);
    if (req.headers['if-none-match'] === tag) {
      res.statusCode = 304;
      res.end();
      return;
    }
  }
  res.end(text);
}

/** The public stream: numbers that wander near the fixtures, every 5 seconds. */
function liveStream(req: IncomingMessage, res: ServerResponse): void {
  res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', Connection: 'keep-alive' });
  res.write('retry: 5000\n\n');
  const send = () => {
    const nodes = load('nodes') as { id: string; clients?: number | null }[];
    const wander = Math.round(Math.sin(Date.now() / 20_000) * 12);
    const counts = Object.fromEntries(nodes.filter((n) => n.clients != null).map((n) => [n.id, Math.max(0, (n.clients ?? 0) + wander)]));
    const total = Object.values(counts).reduce((a, b) => a + b, 0);
    res.write(`event: clients\ndata: ${JSON.stringify({ at: new Date().toISOString(), total, domains: {}, nodes: counts })}\n\n`);
  };
  send();
  const timer = setInterval(send, 5000);
  req.on('close', () => clearInterval(timer));
}

function handle(req: IncomingMessage, res: ServerResponse, next: () => void): void {
  const url = new URL(req.url ?? '/', 'http://localhost');
  if (url.pathname === '/api/v1/stream') {
    liveStream(req, res);
    return;
  }
  if (url.pathname === '/api/v1/openapi.json') {
    res.setHeader('Content-Type', 'application/json');
    res.end(readFileSync(join(web, '..', 'docs', 'openapi.json')));
    return;
  }
  if (url.pathname === '/rapidoc-min.js') {
    res.setHeader('Content-Type', 'text/javascript');
    res.end(readFileSync(join(web, 'node_modules', 'rapidoc', 'dist', 'rapidoc-min.js')));
    return;
  }
  if (!url.pathname.startsWith('/api/v1/')) return next();
  const path = url.pathname.slice('/api/v1'.length);
  if (path.startsWith('/admin/')) {
    void handleAdmin(req, res, path, url.searchParams);
    return;
  }
  const qr = path.match(/^\/links\/([^/]+)\/qr\.svg$/);
  if (qr) {
    res.setHeader('Content-Type', 'image/svg+xml');
    res.end(QR(qr[1]));
    return;
  }
  const { status, body } = answer(path, url.searchParams);
  const text = JSON.stringify(body);
  const tag = `"${createHash('sha1').update(text).digest('hex').slice(0, 16)}"`;
  res.setHeader('ETag', tag);
  res.setHeader('Content-Type', 'application/json');
  if (status === 200 && req.headers['if-none-match'] === tag) {
    res.statusCode = 304;
    res.end();
    return;
  }
  res.statusCode = status;
  res.end(text);
}

export function devApi(): Plugin {
  return {
    name: 'mtpool-dev-api',
    configureServer(server) {
      server.middlewares.use(handle);
    },
    // `vite preview`: the built pages, with the same answers.
    configurePreviewServer(server) {
      server.middlewares.use(handle);
    },
  };
}
