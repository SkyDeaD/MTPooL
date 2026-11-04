// Synthetic answers of the admin API, from tests/fixtures/admin-*.json. Used by `npm run dev`
// (scripts/dev-api.ts) and by the panel tests, so both see the same registrar.
//
// The mock keeps a little state: whether a session is open, and the changes the panel makes
// (settings, owners, tokens, node actions), so the pages can be tried end to end.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const fixtures = join(dirname(fileURLToPath(import.meta.url)), '..', 'tests', 'fixtures');

function load<T = any>(name: string): T {
  return JSON.parse(readFileSync(join(fixtures, `${name}.json`), 'utf8')) as T;
}

export interface MockRequest {
  method: string;
  /** The path after `/api/v1`. */
  path: string;
  query: URLSearchParams;
  /** Lower-case header names. */
  headers: Record<string, string>;
  body?: any;
}

export interface MockAnswer {
  status: number;
  body?: unknown;
}

/** The token the mock accepts at login. */
export const MOCK_TOKEN = 'good-token';

const NOT_FOUND = { code: 'not_found', title: 'Не найдено' };

function merge(into: any, patch: any): any {
  for (const [key, value] of Object.entries(patch)) {
    const there = into[key];
    if (value && typeof value === 'object' && !Array.isArray(value) && there && typeof there === 'object' && !Array.isArray(there)) {
      merge(there, value);
    } else {
      into[key] = value;
    }
  }
  return into;
}

export type Scenario = 'quarantine' | 'calm';

/** The fixtures describe a pool with a node in quarantine and one sensor that sees a block. */
function calmDown(state: { nodes: any[]; status: any; events: any[] }): void {
  for (const n of state.nodes) if (n.state === 'quarantine') n.state = 'healthy';
  for (const s of state.status.sensors) if (s.last_vote === 'blocked') s.last_vote = 'clear';
  state.events = state.events.filter((e) => e.kind !== 'sensor_round');
}

export function createAdminMock(scenario: Scenario = 'quarantine') {
  let session = false;
  const state = {
    nodes: load<any[]>('admin-nodes'),
    rounds: load<any[]>('admin-rounds'),
    status: load<any>('admin-status'),
    srmd: load<any>('admin-srmd'),
    settings: load<any>('admin-settings'),
    owners: load<any[]>('admin-owners'),
    tokens: load<any[]>('admin-tokens'),
    events: load<any[]>('admin-events'),
    audit: load<any[]>('admin-audit'),
    rollouts: load<any[]>('admin-rollouts'),
    domains: load<any>('admin-settings').domains as string[],
  };
  if (scenario === 'calm') calmDown(state);
  let nextId = 100;

  const problem = (status: number, code: string, detail = ''): MockAnswer => ({
    status,
    body: { code, title: code, detail },
  });
  const find = (nodeId: string) => state.nodes.find((n) => n.id === nodeId);

  function settingsBody() {
    return structuredClone(state.settings);
  }

  function handle(req: MockRequest): MockAnswer {
    const { method, path } = req;
    if (path === '/admin/session') {
      if (method === 'POST') {
        if (req.body?.token === MOCK_TOKEN) {
          session = true;
          return { status: 200, body: { csrf: 'csrf-mock' } };
        }
        return problem(401, 'unauthorized');
      }
      if (method === 'DELETE') {
        session = false;
        return { status: 204 };
      }
      return session ? { status: 200, body: { csrf: 'csrf-mock' } } : problem(401, 'unauthorized');
    }
    if (!path.startsWith('/admin/')) return { status: 404, body: NOT_FOUND };
    if (!session) return problem(401, 'unauthorized');
    if (method !== 'GET' && method !== 'HEAD' && req.headers['x-csrf-token'] !== 'csrf-mock') {
      return problem(403, 'csrf');
    }

    const route = path.slice('/admin'.length);
    let m: RegExpMatchArray | null;

    if (route === '/nodes' && method === 'GET') return { status: 200, body: state.nodes };
    if ((m = route.match(/^\/nodes\/([^/]+)$/)) && method === 'GET') {
      const node = find(m[1]);
      return node ? { status: 200, body: { ...node, rounds: node.state === 'pending' ? [] : state.rounds } } : { status: 404, body: NOT_FOUND };
    }
    if ((m = route.match(/^\/nodes\/([^/]+)\/(recheck|retire|unban)$/)) && method === 'POST') {
      const node = find(m[1]);
      if (!node) return { status: 404, body: NOT_FOUND };
      if (m[2] === 'unban') {
        if (node.state !== 'banned') return problem(409, 'not_banned');
        node.state = 'healthy';
      } else if (m[2] === 'retire') {
        node.state = 'pruned';
      } else if (node.state === 'banned') {
        return problem(409, 'banned');
      }
      state.audit.unshift({ id: nextId++, at: new Date().toISOString(), actor: 'admin', action: `node.${m[2]}`, target: node.id, detail: {} });
      return { status: 200, body: {} };
    }
    if (route === '/status') return { status: 200, body: state.status };
    if (route === '/srmd' && method === 'GET') return { status: 200, body: state.srmd };
    if ((m = route.match(/^\/srmd\/([^/]+)\/(take|release)$/)) && method === 'POST') {
      return state.srmd.active.includes(m[1]) || state.srmd.folded.includes(m[1]) ? { status: 200, body: {} } : { status: 404, body: NOT_FOUND };
    }
    if (route === '/settings') {
      if (method === 'GET') return { status: 200, body: settingsBody() };
      if (method === 'PATCH') {
        const patch = structuredClone(req.body ?? {});
        const secrets = patch.secrets;
        delete patch.secrets;
        const next = merge(structuredClone(state.settings), patch);
        if (typeof next.heartbeat_interval_sec !== 'number') return problem(400, 'bad_request');
        for (const [key, value] of Object.entries<number>(next)) {
          if (key.endsWith('_sec') && !(value > 0)) return problem(400, 'invalid_settings', `${key}: должно быть больше нуля`);
        }
        if (secrets) {
          for (const [name, value] of Object.entries<string | null>(secrets)) next.secrets[name] = value !== null;
        }
        state.settings = next;
        state.domains = next.domains;
        state.audit.unshift({ id: nextId++, at: new Date().toISOString(), actor: 'admin', action: 'settings.update', detail: { fields: Object.keys(patch) } });
        return { status: 200, body: settingsBody() };
      }
    }
    if (route === '/domains') {
      if (method === 'GET') return { status: 200, body: state.domains };
      if (method === 'POST') {
        const name = String(req.body?.domain ?? '');
        if (!name) return problem(400, 'bad_request');
        if (state.domains.includes(name)) return problem(409, 'exists');
        state.domains.push(name);
        return { status: 201, body: {} };
      }
    }
    if ((m = route.match(/^\/domains\/([^/]+)$/)) && method === 'DELETE') {
      state.domains = state.domains.filter((d) => d !== m![1]);
      return { status: 200, body: {} };
    }
    if (route === '/dns/push' && method === 'POST') return { status: 202 };
    if (route === '/owners') {
      if (method === 'GET') return { status: 200, body: state.owners };
      if (method === 'POST') {
        const b = req.body ?? {};
        if (!b.name) return problem(400, 'bad_request');
        if (state.owners.some((o) => o.name === b.name)) return problem(409, 'exists');
        const owner = { id: nextId++, name: b.name, node_limit: b.node_limit ?? 0, note: b.note ?? '', created_at: new Date().toISOString() };
        state.owners.push(owner);
        return { status: 201, body: owner };
      }
    }
    if ((m = route.match(/^\/owners\/(\d+)$/))) {
      const owner = state.owners.find((o) => o.id === Number(m![1]));
      if (!owner) return { status: 404, body: NOT_FOUND };
      if (method === 'PATCH') {
        Object.assign(owner, Object.fromEntries(Object.entries(req.body ?? {}).filter(([, v]) => v !== null)));
        return { status: 200, body: owner };
      }
      if (method === 'DELETE') {
        if (state.nodes.some((n) => n.owner_id === owner.id)) return problem(409, 'owner_has_nodes');
        state.owners = state.owners.filter((o) => o !== owner);
        return { status: 200, body: {} };
      }
    }
    if (route === '/tokens') {
      if (method === 'GET') {
        const owner = req.query.get('owner');
        return { status: 200, body: owner ? state.tokens.filter((t) => String(t.owner_id) === owner) : state.tokens };
      }
      if (method === 'POST') {
        const b = req.body ?? {};
        const id = nextId++;
        const meta = { id, kind: b.kind, prefix: `mtp_${String(b.kind).slice(0, 3)}_${id}`, scope: b.scope ?? '', owner_id: b.owner_id ?? undefined, expires_at: b.expires_at ?? undefined, created_at: new Date().toISOString() };
        state.tokens.push(meta);
        return { status: 201, body: { ...meta, token: `mtp_${b.kind}_secret${id}` } };
      }
    }
    if ((m = route.match(/^\/tokens\/(\d+)$/)) && method === 'DELETE') {
      const token = state.tokens.find((t) => t.id === Number(m![1]));
      if (!token) return { status: 404, body: NOT_FOUND };
      token.revoked_at = new Date().toISOString();
      return { status: 200, body: {} };
    }
    if (route === '/events') {
      const kind = req.query.get('kind');
      const node = req.query.get('node');
      const rows = state.events.filter((e) => (!kind || e.kind === kind) && (!node || e.node === node));
      const limit = Number(req.query.get('limit'));
      return { status: 200, body: limit > 0 ? rows.slice(0, limit) : rows };
    }
    if (route === '/audit') return { status: 200, body: state.audit };
    if (route === '/rollouts') return { status: 200, body: state.rollouts };
    if ((m = route.match(/^\/rollouts\/(\d+)(?:\/(resume|cancel))?$/))) {
      const rollout = state.rollouts.find((r) => r.id === Number(m![1]));
      if (!rollout) return { status: 404, body: NOT_FOUND };
      if (!m[2] && method === 'GET') return { status: 200, body: rollout };
      if (m[2] === 'resume' && method === 'POST') {
        if (!rollout.can_resume) return problem(409, 'not_paused');
        rollout.phase = rollout.resume_to ?? 'batches';
        rollout.can_resume = false;
        return { status: 200, body: rollout };
      }
      if (m[2] === 'cancel' && method === 'POST') {
        if (!rollout.can_cancel) return problem(409, 'rollout_over');
        rollout.phase = 'cancelled';
        rollout.can_cancel = false;
        rollout.can_resume = false;
        return { status: 200, body: rollout };
      }
    }
    return { status: 404, body: NOT_FOUND };
  }

  return {
    state,
    handle,
    /** Ends the session on the server side, as an expiry does. */
    expire() {
      session = false;
    },
    get loggedIn() {
      return session;
    },
  };
}

export type AdminMock = ReturnType<typeof createAdminMock>;
