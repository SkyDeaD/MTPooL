// What the pages work out from API answers. Pure functions: no time of their own, no network.
import { t } from './i18n';
import { messages, type MessageKey } from '../strings.ru';
import type { Engine, NodeChecks, NodeHistory, PublicDomain, PublicNode } from './types';

export const ENGINE_NAMES: Record<Engine, string> = {
  telemt: 'telemt',
  mt_proxy_l: 'MTProxyL',
  meko: 'MEKO',
};

export interface NodeGroups {
  masters: PublicNode[];
  queue: PublicNode[];
  quarantine: PublicNode[];
}

/**
 * The nodes the pool shows, in three groups. Nodes that are pending, dead or pruned are neither
 * shown nor counted; banned nodes do not come in this list at all.
 */
export function groupNodes(nodes: readonly PublicNode[]): NodeGroups {
  const masters: PublicNode[] = [];
  const queue: PublicNode[] = [];
  const quarantine: PublicNode[] = [];
  for (const node of nodes) {
    if (node.state === 'quarantine') quarantine.push(node);
    else if (node.state === 'healthy') (node.is_master ? masters : queue).push(node);
  }
  const place = (n: PublicNode) => n.queue_position ?? Number.MAX_SAFE_INTEGER;
  queue.sort((a, b) => place(a) - place(b));
  return { masters, queue, quarantine };
}

export interface PoolCounts {
  /** Nodes shown in the table. */
  shown: number;
  /** Of them, the healthy ones. */
  working: number;
  /** Mean availability in hundredths of a percent, over nodes that have a figure. */
  availabilityBp: number | null;
}

export function poolCounts(nodes: readonly PublicNode[]): PoolCounts {
  const g = groupNodes(nodes);
  const all = [...g.masters, ...g.queue, ...g.quarantine];
  const known = all.map((n) => n.availability_bp).filter((v): v is number => typeof v === 'number');
  return {
    shown: all.length,
    working: g.masters.length + g.queue.length,
    availabilityBp: known.length > 0 ? known.reduce((a, b) => a + b, 0) / known.length : null,
  };
}

/** "99,87 %" from hundredths of a percent; a dash when there is no figure. */
export function formatAvailability(bp: number | null | undefined): string {
  if (bp === null || bp === undefined) return '—';
  const percent = bp / 100;
  const text = Number.isInteger(percent) ? String(percent) : percent.toFixed(2).replace('.', ',');
  return `${text} %`;
}

export type CheckProblem = 'tcp' | 'globalping' | 'metrics' | 'globalping_nodata';

/** The checks that did not pass, in the order the table lists them. */
export function checksProblems(checks: NodeChecks): CheckProblem[] {
  const out: CheckProblem[] = [];
  if (!checks.tcp) out.push('tcp');
  if (checks.globalping === 'failed') out.push('globalping');
  else if (checks.globalping !== 'ok') out.push('globalping_nodata');
  if (!checks.metrics) out.push('metrics');
  return out;
}

/** The first part of a domain: "proxy1.example.com" is "proxy1". */
export function shortDomain(domain: string): string {
  return domain.split('.')[0] ?? domain;
}

/** "12:34" for a count of seconds; an hour or more reads "1:02:03". Negative counts as zero. */
export function clock(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const two = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${two(m)}:${two(s % 60)}` : `${two(m)}:${two(s % 60)}`;
}

/** Seconds from `nowMs` to an RFC 3339 instant; null when there is none. */
export function secondsUntil(at: string | null | undefined, nowMs: number): number | null {
  if (!at) return null;
  const ms = Date.parse(at);
  return Number.isNaN(ms) ? null : (ms - nowMs) / 1000;
}

export type SlotState = 'ok' | 'warn' | 'bad' | 'none';

export interface HistorySlot {
  start: number;
  state: SlotState;
  clients: number | null;
}

export const HISTORY_SLOTS = 96;

/**
 * 96 slots of one bucket each, ending with the current one, oldest first. A bucket the API did not
 * send has no samples: it is "no data", which is not the same as down.
 */
export function historySlots(history: NodeHistory, nowMs: number): HistorySlot[] {
  const size = history.bucket_sec * 1000;
  const current = Math.floor(nowMs / size) * size;
  const byStart = new Map(history.buckets.map((b) => [Date.parse(b.start), b]));
  const slots: HistorySlot[] = [];
  for (let i = 0; i < HISTORY_SLOTS; i++) {
    const start = current - (HISTORY_SLOTS - 1 - i) * size;
    const b = byStart.get(start);
    let state: SlotState = 'none';
    if (b && b.total > 0) state = b.up >= b.total ? 'ok' : b.up === 0 ? 'bad' : 'warn';
    slots.push({ start, state, clients: b && b.total > 0 ? b.clients : null });
  }
  return slots;
}

export interface SchemeDomain {
  domain: string;
  clients: number | null;
  masters: PublicNode[];
  /** Domains folded into this one: they have no masters of their own. */
  folded: PublicDomain[];
}

export interface Scheme {
  domains: SchemeDomain[];
  queue: PublicNode[];
}

/** The pool as the scheme draws it: domains with their masters, folded domains under their host. */
export function buildScheme(domains: readonly PublicDomain[], nodes: readonly PublicNode[]): Scheme {
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const names = new Set(domains.map((d) => d.domain));
  const isFolded = (d: PublicDomain) => !!d.folded_into && names.has(d.folded_into);
  const hosts = domains.filter((d) => !isFolded(d));
  return {
    domains: hosts.map((d) => ({
      domain: d.domain,
      clients: d.clients ?? null,
      masters: d.masters.map((id) => byId.get(id)).filter((n): n is PublicNode => !!n),
      folded: domains.filter((f) => isFolded(f) && f.folded_into === d.domain),
    })),
    queue: groupNodes(nodes).queue,
  };
}

/** The name of a sensor as the page writes it; a sensor the page does not know keeps its own name. */
export function sensorName(sensor: string): string {
  const key = `sensor.${sensor}`;
  return key in messages ? t(key as MessageKey) : sensor;
}

/** "Globalping + cheburcheck" from the API's `cheburcheck+globalping`. */
export function reasonLabel(reason: string): string {
  return reason.split('+').map(sensorName).join(' + ');
}
