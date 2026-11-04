// Makes src/public/api-routes.generated.ts from docs/openapi.json: the list of routes the API page
// shows. A route without `security` is open to anyone; a route under /api/v1/me with a bearer token
// is for bots. The admin, agent and metrics routes are never listed. The descriptions are not here:
// they are Russian texts in src/strings.ru.ts, keyed `api.route.<METHOD> <path>`.
import { readFile, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export interface ApiRoute {
  /** Upper case: GET, POST, DELETE. */
  method: string;
  path: string;
  /** Names of the query parameters, in the order the document lists them. */
  query: string[];
}

interface Operation {
  security?: unknown;
  parameters?: { name: string; in: string }[];
}
interface OpenApi {
  paths: Record<string, Record<string, Operation>>;
}

const METHODS = ['get', 'post', 'put', 'patch', 'delete'];
// The order the page lists open routes in: what people ask for most goes first. The document itself
// is sorted by path, which would open the page with the ban history.
const PUBLIC_ORDER = ['/api/v1/nodes', '/api/v1/domains', '/api/v1/links', '/api/v1/bans', '/api/v1/stats'];
const rank = (path: string): number => {
  const i = PUBLIC_ORDER.findIndex((prefix) => path.startsWith(prefix));
  return i === -1 ? PUBLIC_ORDER.length : i;
};

export function buildApiRoutes(openapi: OpenApi): { pub: ApiRoute[]; bot: ApiRoute[] } {
  const pub: ApiRoute[] = [];
  const bot: ApiRoute[] = [];
  for (const [path, item] of Object.entries(openapi.paths)) {
    if (path.startsWith('/api/v1/admin') || path.startsWith('/api/v1/agent') || path === '/metrics') continue;
    for (const method of METHODS) {
      const op = item[method];
      if (!op) continue;
      const route: ApiRoute = {
        method: method.toUpperCase(),
        path,
        query: (op.parameters ?? []).filter((p) => p.in === 'query').map((p) => p.name),
      };
      if (!op.security) pub.push(route);
      else if (path.startsWith('/api/v1/me')) bot.push(route);
    }
  }
  pub.sort((x, y) => rank(x.path) - rank(y.path));
  return { pub, bot };
}

export function renderModule(routes: { pub: ApiRoute[]; bot: ApiRoute[] }): string {
  return [
    '// Made by scripts/gen-api-docs.ts from docs/openapi.json. Do not edit by hand.',
    "export interface ApiRoute {",
    '  method: string;',
    '  path: string;',
    '  query: string[];',
    '}',
    '',
    `export const publicRoutes: ApiRoute[] = ${JSON.stringify(routes.pub, null, 2)};`,
    '',
    `export const botRoutes: ApiRoute[] = ${JSON.stringify(routes.bot, null, 2)};`,
    '',
  ].join('\n');
}

// Run as a script, not when a test imports it.
if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const root = join(dirname(fileURLToPath(import.meta.url)), '..');
  const openapi = JSON.parse(await readFile(join(root, '..', 'docs', 'openapi.json'), 'utf8')) as OpenApi;
  const routes = buildApiRoutes(openapi);
  await writeFile(join(root, 'src', 'public', 'api-routes.generated.ts'), renderModule(routes));
  console.log(`api routes: ${routes.pub.length} public, ${routes.bot.length} for bots`);
}
