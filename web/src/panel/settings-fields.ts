// Every setting the server has, in the order the page shows them. A key is the server's own
// (dotted for nested groups); the page never shows it, only the label from the strings file.

export type Kind = 'sec' | 'count' | 'percent' | 'ratio' | 'bool' | 'text' | 'port' | 'domains' | 'users';

export interface Field {
  key: string;
  kind: Kind;
  min?: number;
  max?: number;
}

export interface Group {
  id: string;
  fields: Field[];
}

const sec = (key: string): Field => ({ key, kind: 'sec', min: 1, max: 86400 });
const count = (key: string, max = 1000): Field => ({ key, kind: 'count', min: 1, max });

export const GROUPS: Group[] = [
  {
    id: 'checks',
    fields: [
      sec('heartbeat_interval_sec'),
      sec('heartbeat_ttl_sec'),
      sec('report_max_age_sec'),
      sec('metrics_interval_sec'),
      sec('sync_interval_sec'),
      sec('tcp_probe_interval_sec'),
      sec('tcp_probe_timeout_sec'),
      count('latch_fail_threshold'),
      count('latch_ok_threshold'),
      sec('dead_after_sec'),
    ],
  },
  {
    id: 'quarantine',
    fields: [
      sec('quarantine_max_sec'),
      sec('round_interval_sec'),
      sec('round_interval_slow_sec'),
      count('rounds_before_slow'),
      count('clean_rounds_to_exit'),
      sec('reverify_pause_sec'),
    ],
  },
  { id: 'masters', fields: [count('masters_per_domain', 50), sec('rotation_ttl_sec')] },
  {
    id: 'dns',
    fields: [sec('dns_ttl'), { key: 'dns_proxied', kind: 'bool' }, { key: 'domains', kind: 'domains' }],
  },
  {
    id: 'proxy',
    fields: [{ key: 'tls_domain', kind: 'text' }, { key: 'proxy_port', kind: 'port', min: 1, max: 65535 }, { key: 'proxy_users', kind: 'users' }],
  },
  {
    id: 'globalping',
    fields: [
      sec('globalping_interval_sec'),
      count('globalping.probe_limit'),
      count('globalping.min_probes'),
      { key: 'globalping.success_threshold', kind: 'ratio', min: 0, max: 1 },
      sec('globalping.vote_report_max_age_sec'),
      sec('globalping.window_past_sec'),
      sec('globalping.window_future_sec'),
      count('globalping.min_limit_remaining', 100000),
      { key: 'globalping.allow_credits', kind: 'bool' },
    ],
  },
  {
    id: 'srmd',
    fields: [
      { key: 'srmd.enabled', kind: 'bool' },
      { key: 'srmd.base_domain', kind: 'text' },
      count('srmd.max_nodes_per_domain', 50),
      count('srmd.max_domains', 50),
      count('srmd.diverge_ticks'),
    ],
  },
  {
    id: 'rollout',
    fields: [
      sec('rollout.observe_sec'),
      sec('rollout.batch_interval_sec'),
      { key: 'rollout.batch_percent', kind: 'percent', min: 1, max: 100 },
      sec('rollout.return_sec'),
      sec('rollout.offer_wait_sec'),
    ],
  },
];

/** The secrets, in the order the page shows them. */
export const SECRETS = ['globalping_token', 'redflag_api_key', 'cloudflare_api_token'] as const;

export type Value = string | boolean;
export type Values = Record<string, Value>;

export function pick(doc: Record<string, unknown>, key: string): unknown {
  return key.split('.').reduce<unknown>((o, k) => (o && typeof o === 'object' ? (o as Record<string, unknown>)[k] : undefined), doc);
}

export function toText(field: Field, raw: unknown): Value {
  if (field.kind === 'bool') return raw === true;
  if (field.kind === 'domains') return Array.isArray(raw) ? raw.join('\n') : '';
  if (field.kind === 'users')
    return Array.isArray(raw) ? raw.map((u: { name: string; secret_hex: string }) => `${u.name}:${u.secret_hex}`).join('\n') : '';
  return raw === undefined || raw === null ? '' : String(raw);
}

export function seed(doc: Record<string, unknown>): Values {
  const out: Values = {};
  for (const g of GROUPS) for (const f of g.fields) out[f.key] = toText(f, pick(doc, f.key));
  return out;
}

export type Problem = { code: 'int' | 'float' | 'empty' | 'users' | 'domains'; min?: number; max?: number };

export function parse(field: Field, value: Value): { ok: true; value: unknown } | { ok: false; problem: Problem } {
  const text = typeof value === 'string' ? value.trim() : '';
  switch (field.kind) {
    case 'bool':
      return { ok: true, value: value === true };
    case 'text':
      return text ? { ok: true, value: text } : { ok: false, problem: { code: 'empty' } };
    case 'domains': {
      const list = text.split(/\s+/).filter(Boolean);
      return list.length > 0 ? { ok: true, value: list } : { ok: false, problem: { code: 'domains' } };
    }
    case 'users': {
      const lines = text.split('\n').map((l) => l.trim()).filter(Boolean);
      const users = lines.map((l) => {
        const [name, secret_hex] = l.split(':').map((p) => p.trim());
        return { name, secret_hex };
      });
      const good = users.length > 0 && users.every((u) => u.name && /^[0-9a-fA-F]{32}$/.test(u.secret_hex ?? ''));
      return good ? { ok: true, value: users } : { ok: false, problem: { code: 'users' } };
    }
    case 'ratio': {
      const n = Number(text.replace(',', '.'));
      return text !== '' && Number.isFinite(n) && n >= (field.min ?? 0) && n <= (field.max ?? 1)
        ? { ok: true, value: n }
        : { ok: false, problem: { code: 'float', min: field.min, max: field.max } };
    }
    default: {
      const n = Number(text);
      return text !== '' && Number.isInteger(n) && n >= (field.min ?? 1) && n <= (field.max ?? 86400)
        ? { ok: true, value: n }
        : { ok: false, problem: { code: 'int', min: field.min, max: field.max } };
    }
  }
}

/** Puts `value` at the dotted `key` of `target`, making the groups on the way. */
export function put(target: Record<string, unknown>, key: string, value: unknown): void {
  const parts = key.split('.');
  let at = target;
  for (const p of parts.slice(0, -1)) at = (at[p] ??= {}) as Record<string, unknown>;
  at[parts[parts.length - 1]] = value;
}
