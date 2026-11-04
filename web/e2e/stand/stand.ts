// A real registrar serving the real `web/dist`, with the stand-ins of the outside services behind
// it and a handful of simulated nodes that talk to it over the agent API. The browser checks and
// the performance run both start it; nothing here is the Vite mock.
//
// What is real: the registrar binary, its database, the state machine, the sensors, the DNS
// worker, the server-sent events. What is simulated: the nodes. One thing is arranged by hand:
// the registrar probes a node's TCP port from outside, and a documentation address (203.0.113.x)
// has nothing listening. So after the nodes enroll, the registrar is stopped, the TCP latch of the
// nodes is set to "up" in its database, and it is started again with probing slowed down.
//
// The admin token is made by the registrar at start (`admin reset-token`) and lives only in this
// process and in the environment of the test workers. It is never printed.
import { type ChildProcess, execFileSync, spawn } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { DatabaseSync } from 'node:sqlite';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Agent, type SimNode, type FakesMode } from './agent.ts';

const here = dirname(fileURLToPath(import.meta.url));
export const WEB_DIR = resolve(here, '..', '..');
const REPO = resolve(WEB_DIR, '..');

export const DOMAINS = ['alpha.example.com', 'beta.example.com'];
export const TLS_DOMAIN = 'tls.example.com';
const SECRET_HEX = '00112233445566778899aabbccddeeff';

/** What the nodes of the stand are, by name. The names are what the pages show. */
export interface Roster {
  /** Healthy nodes: the first two become masters of the two domains. */
  healthy: string[];
  queued: string;
  quarantine: string;
  banned: string;
}

export interface Stand {
  base: string;
  fakes: string;
  /** The admin token. Pass it on, never print it. */
  adminToken: string;
  enrollToken: string;
  roster: Roster;
  /** The ids registrar gave the nodes, by the names above. */
  ids: Record<string, string>;
  agent: Agent;
  nodes: Map<string, SimNode>;
  admin(path: string, init?: { method?: string; body?: unknown }): Promise<any>;
  stop(): Promise<void>;
}

export function ports(): { registrar: number; fakes: number } {
  return {
    registrar: Number(process.env.MTPOOL_STAND_PORT ?? 18480),
    fakes: Number(process.env.MTPOOL_STAND_FAKES_PORT ?? 19480),
  };
}

function binDir(): string {
  const target = process.env.CARGO_TARGET_DIR ?? join(REPO, 'target');
  return join(target, process.env.MTPOOL_PROFILE ?? 'debug');
}

/** Builds the two binaries (a no-op when they are current). The registrar embeds `web/dist`. */
export function buildBinaries(): void {
  if (!existsSync(join(WEB_DIR, 'dist', 'panel', 'index.html'))) {
    throw new Error('web/dist is not built: run `npm run build` in web/ first');
  }
  if (process.env.MTPOOL_SKIP_BUILD === '1') return;
  const profile = process.env.MTPOOL_PROFILE === 'release' ? ['--release'] : [];
  execFileSync('cargo', ['build', '-p', 'mtpool-registrar', '-p', 'mtpool-fakes', '--locked', ...profile], {
    cwd: REPO,
    stdio: ['ignore', 'inherit', 'inherit'],
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function until<T>(what: string, check: () => Promise<T | undefined | false>, timeoutMs = 30_000): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  while (Date.now() < deadline) {
    try {
      const got = await check();
      if (got) return got;
    } catch (e) {
      last = e;
    }
    await sleep(250);
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${String(last)}` : ''}`);
}

function start(bin: string, env: Record<string, string>, args: string[], log: string[]): ChildProcess {
  const child = spawn(join(binDir(), bin), args, {
    env: { PATH: process.env.PATH ?? '', ...env },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const keep = (chunk: Buffer) => {
    log.push(chunk.toString());
    if (log.length > 200) log.shift();
  };
  child.stdout?.on('data', keep);
  child.stderr?.on('data', keep);
  return child;
}

function stopChild(child: ChildProcess): Promise<void> {
  return new Promise((done) => {
    if (child.exitCode !== null || child.signalCode !== null) return done();
    const timer = setTimeout(() => child.kill('SIGKILL'), 8000);
    child.once('exit', () => {
      clearTimeout(timer);
      done();
    });
    child.kill('SIGTERM');
  });
}

export async function startStand(): Promise<Stand> {
  buildBinaries();
  const { registrar: rport, fakes: fport } = ports();
  const base = `http://127.0.0.1:${rport}`;
  const fakes = `http://127.0.0.1:${fport}`;
  const dir = mkdtempSync(join(tmpdir(), 'mtpool-browser-'));
  const db = join(dir, 'registrar.db');
  const log: string[] = [];
  const env = {
    MTPOOL_DB_PATH: db,
    MTPOOL_LISTEN: `127.0.0.1:${rport}`,
    MTPOOL_LOG: 'warn',
    MTPOOL_RELEASES_DIR: join(dir, 'releases'),
    MTPOOL_GLOBALPING_URL: `${fakes}/globalping/`,
    MTPOOL_CHEBURCHECK_URL: `${fakes}/cheburcheck/`,
    MTPOOL_REDFLAG_URL: `${fakes}/redflag/`,
    MTPOOL_CLOUDFLARE_URL: `${fakes}/cloudflare/`,
  };

  const fakesProc = start('mtpool-fakes', { MTPOOL_FAKES_LISTEN: `127.0.0.1:${fport}` }, [], log);
  let registrar: ChildProcess | undefined;
  const bring = async () => {
    registrar = start('mtpool-registrar', env, ['serve'], log);
    await until('the registrar', async () => (await fetch(`${base}/readyz`)).ok);
  };
  const timers: ReturnType<typeof setInterval>[] = [];
  const cleanup = async () => {
    timers.forEach(clearInterval);
    if (registrar) await stopChild(registrar);
    await stopChild(fakesProc);
    rmSync(dir, { recursive: true, force: true });
  };

  try {
    await until('the fakes', async () => (await fetch(`${fakes}/__state`)).ok);
    await bring();
    const adminToken = execFileSync(join(binDir(), 'mtpool-registrar'), ['admin', 'reset-token'], {
      env: { PATH: process.env.PATH ?? '', MTPOOL_DB_PATH: db, MTPOOL_LOG: 'error' },
      encoding: 'utf8',
    }).trim();
    if (!adminToken) throw new Error('admin reset-token printed no token');

    const admin: Stand['admin'] = async (path, init = {}) => {
      const res = await fetch(`${base}/api/v1/admin${path}`, {
        method: init.method ?? (init.body === undefined ? 'GET' : 'POST'),
        headers: { authorization: `Bearer ${adminToken}`, 'content-type': 'application/json' },
        body: init.body === undefined ? undefined : JSON.stringify(init.body),
      });
      const text = await res.text();
      if (!res.ok) throw new Error(`${init.method ?? (init.body === undefined ? 'GET' : 'POST')} ${path}: ${res.status} ${text}`);
      return text ? JSON.parse(text) : null;
    };

    // The pool reacts in seconds. Probing is slowed to once a day and the latch needs 255 failures
    // to fall, because nothing listens on the documentation addresses of the nodes.
    await admin('/settings', {
      method: 'PATCH',
      body: {
        heartbeat_interval_sec: 2,
        globalping_interval_sec: 5,
        metrics_interval_sec: 2,
        sync_interval_sec: 2,
        heartbeat_ttl_sec: 20,
        tcp_probe_interval_sec: 86_400,
        latch_fail_threshold: 255,
        round_interval_sec: 2,
        round_interval_slow_sec: 3_600,
        dead_after_sec: 600,
        masters_per_domain: 1,
        dns_ttl: 60,
        domains: DOMAINS,
        tls_domain: TLS_DOMAIN,
        proxy_port: 443,
        proxy_users: [{ name: 'main', secret_hex: SECRET_HEX }],
        secrets: { globalping_token: 'stand-token', redflag_api_key: 'stand-token', cloudflare_api_token: 'stand-token' },
      },
    });
    const owner = await admin('/owners', { body: { name: 'stand-owner', node_limit: 50, note: 'browser checks' } });
    const enrollToken: string = (await admin('/tokens', { body: { kind: 'enroll', owner_id: owner.id } })).token;
    await admin('/owners', { body: { name: 'second-owner', node_limit: 5 } });
    await admin('/tokens', { body: { kind: 'bot', owner_id: owner.id } });

    const agent = new Agent(base, fakes, enrollToken);
    const roster: Roster = {
      healthy: ['alpha-1', 'beta-1', 'gamma-1', 'delta-1', 'omega-1'],
      queued: 'queue-1',
      quarantine: 'sick-1',
      banned: 'blocked-1',
    };
    const nodes = new Map<string, SimNode>();
    const addresses: [string, string][] = [
      ...roster.healthy.map((n, i) => [n, `203.0.113.${10 + i}`] as [string, string]),
      [roster.queued, '203.0.113.30'],
      [roster.quarantine, '203.0.113.40'],
      [roster.banned, '203.0.113.50'],
    ];
    for (const [name, ip] of addresses) nodes.set(name, await agent.enroll(name, ip, 443));

    // The latch of every node goes up while the registrar is stopped; it reads it back on start.
    await stopChild(registrar!);
    const conn = new DatabaseSync(db);
    for (const row of conn.prepare('SELECT id, record, health FROM nodes').all() as { id: string; record: string; health: string | null }[]) {
      const fix = (json: string | null, whole: boolean) => {
        if (!json) return json;
        const doc = JSON.parse(json);
        const health = whole ? doc.health : doc;
        if (health?.tcp) health.tcp = { up: true, fails: 0, oks: 0 };
        return JSON.stringify(doc);
      };
      conn.prepare('UPDATE nodes SET record = ?, health = ? WHERE id = ?').run(fix(row.record, true)!, fix(row.health, false), row.id);
    }
    conn.close();
    await bring();

    // Heartbeats and reports go on, so the pool stays as it is while the tests run.
    const cleanSet = [...roster.healthy];
    const beat = setInterval(() => {
      for (const name of cleanSet) void agent.heartbeat(nodes.get(name)!).catch(() => {});
      void agent.heartbeat(nodes.get(roster.queued)!).catch(() => {});
      void agent.heartbeat(nodes.get(roster.quarantine)!).catch(() => {});
      void agent.heartbeat(nodes.get(roster.banned)!).catch(() => {});
    }, 3000);
    let quiet = false;
    const refresh = setInterval(() => {
      if (quiet) return;
      cleanSet.forEach((name, i) => void agent.report(nodes.get(name)!, { clients: 40 + i * 25 }).catch(() => {}));
    }, 20_000);
    timers.push(beat, refresh);
    const ids: Record<string, string> = {};
    for (const [name, node] of nodes) ids[name] = node.id;

    const stand: Stand = {
      base,
      fakes,
      adminToken,
      enrollToken,
      roster,
      ids,
      agent,
      nodes,
      admin,
      stop: async () => {
        await cleanup();
      },
    };

    // The healthy ones: a clean report, then a clean report now and then.
    await Promise.all(cleanSet.map((n, i) => agent.report(nodes.get(n)!, { clients: 40 + i * 25 })));
    await sleep(2500);
    await Promise.all(cleanSet.map((n, i) => agent.report(nodes.get(n)!, { clients: 40 + i * 25 })));
    let seen = '';
    await until('the masters', async () => {
      const list = await admin('/nodes');
      seen = JSON.stringify(list).slice(0, 1500);
      return list.filter((n: any) => n.state === 'healthy').length >= cleanSet.length - 1;
    }, 40_000).catch((e) => {
      throw new Error(`${e}\n${seen}`);
    });

    // While the sensors are made to lie, healthy nodes send no fresh reports (a report is a check).
    quiet = true;
    // The node that gets banned: both Globalping and cheburcheck see a block.
    await agent.fakes('globalping', 'blocked');
    await agent.fakes('cheburcheck', 'blocked');
    await agent.report(nodes.get(roster.banned)!, { clients: 0 });
    await sleep(2500);
    await agent.report(nodes.get(roster.banned)!, { clients: 0 }).catch(() => {}); // may be banned already
    await until(
      'the ban',
      async () =>
        ((await (await fetch(`${base}/api/v1/bans`)).json()) as { node_id: string }[]).some((b) => b.node_id === ids[roster.banned]) ||
        undefined,
      40_000,
    ).catch(async (e) => {
      const list = await admin('/nodes');
      const mine = list.find((n: any) => n.id === ids[roster.banned]);
      throw new Error(`${e}\n${JSON.stringify(mine)}\n${JSON.stringify(await (await fetch(`${fakes}/__state`)).json()).slice(0, 600)}`);
    });

    // The node that stays in quarantine: one sensor sees a block, the other two cannot say.
    await agent.fakes('cheburcheck', 'unavailable');
    await agent.fakes('redflag', 'unauthorized');
    await agent.report(nodes.get(roster.quarantine)!, { clients: 0 });
    await sleep(2500);
    await agent.report(nodes.get(roster.quarantine)!, { clients: 0 });
    await until('the quarantine', async () => {
      const list = await admin('/nodes');
      return list.some((n: any) => n.state === 'quarantine') || undefined;
    });
    await sleep(6000); // a round or two, so the node page has votes to show
    await admin('/settings', { method: 'PATCH', body: { round_interval_sec: 3_600 } });
    for (const sensor of ['globalping', 'cheburcheck', 'redflag'] as FakesMode[]) await agent.fakes(sensor, 'ok');
    quiet = false;

    // The node that waits in the queue: pending, never reported.
    // (It enrolled and was latched up above, but sends no report, so it stays Pending.)

    return stand;
  } catch (e) {
    await cleanup();
    throw new Error(`${String(e)}\n--- logs of the stand\n${log.join('').slice(-4000)}`);
  }
}
