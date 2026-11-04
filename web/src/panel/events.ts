// Journal entries in words. The server sends a kind and a few fields; the panel never shows them raw.
import { formatNumber } from '../lib/dates';
import { messages, t, type Key } from './i18n';
import type { AuditEntry, EventEntry } from './types';

export type Tone = 'ok' | 'warn' | 'bad' | 'acc';
export type Category = 'masters' | 'dns' | 'bans' | 'nodes' | 'settings';

export const CATEGORIES: readonly Category[] = ['masters', 'dns', 'bans', 'nodes', 'settings'];

const CATEGORY: Record<string, Category> = {
  master_changed: 'masters',
  dns_applied: 'dns',
  dns_failed: 'dns',
  node_banned: 'bans',
  node_reverify_failed: 'bans',
  ban_lifted: 'bans',
  sensor_round: 'bans',
  node_registered: 'nodes',
  node_quarantined: 'nodes',
  node_recovered: 'nodes',
  node_dead: 'nodes',
  node_pruned: 'nodes',
};

const TONE: Record<string, Tone> = {
  node_registered: 'ok',
  node_recovered: 'ok',
  dns_applied: 'ok',
  ban_lifted: 'ok',
  node_quarantined: 'warn',
  node_reverify_failed: 'warn',
  clock_skew: 'warn',
  node_banned: 'bad',
  node_dead: 'bad',
  dns_failed: 'bad',
  store_error: 'bad',
  master_changed: 'acc',
  rollout_changed: 'acc',
  srmd_action: 'acc',
};

export interface Described {
  tone: Tone;
  icon: 'check' | 'alert' | 'stop' | 'ref';
  title: string;
  detail: string;
  category: Category;
  /** Everything a search can match: the words shown and the names behind them. */
  haystack: string;
}

const ICON = { ok: 'check', warn: 'alert', bad: 'stop', acc: 'ref' } as const;

const known = (key: string): key is Key => key in messages;

export function verdictText(verdict: unknown): string {
  const key = `panel.verdict.${String(verdict)}`;
  return known(key) ? t(key) : '';
}

export function describeEvent(e: EventEntry): Described {
  const d = e.detail as Record<string, unknown>;
  const node = e.node ?? '';
  const domain = e.domain ?? '';
  let tone: Tone = TONE[e.kind] ?? 'acc';
  let title: string;
  let detail = '';
  switch (e.kind) {
    case 'sensor_round': {
      title = t('panel.ev.round', { node });
      detail = verdictText(d.verdict);
      tone = d.verdict === 'clean' ? 'ok' : d.verdict === 'confirmed' ? 'bad' : 'warn';
      break;
    }
    case 'master_changed':
      title = t('panel.ev.master', { domain });
      detail = d.from ? t('panel.ev.masterDetail', { node, from: String(d.from) }) : node;
      break;
    case 'node_banned':
      title = t('panel.ev.banned', { node });
      detail = typeof d.votes === 'number' ? t('panel.ev.bannedDetail', { n: d.votes }) : '';
      break;
    case 'dns_applied':
      title = t('panel.ev.dns', { domain });
      detail = typeof d.records === 'number' ? t('panel.ev.dnsDetail', { n: d.records }) : '';
      break;
    case 'node_dead':
      title = t('panel.ev.dead', { node });
      break;
    default: {
      const key = `panel.event.${e.kind}`;
      title = known(key) ? t(key) : t('panel.event.other');
      detail = [node, domain].filter(Boolean).join(', ');
    }
  }
  return {
    tone,
    icon: ICON[tone],
    title,
    detail,
    category: CATEGORY[e.kind] ?? 'settings',
    haystack: `${title} ${detail} ${node} ${domain}`.toLowerCase(),
  };
}

/** What a person did, in words; fields the panel does not know are left out. */
export function describeAudit(a: AuditEntry): { what: string; target: string } {
  const key = `panel.audit.${a.action}`;
  const d = a.detail as Record<string, unknown>;
  let what = known(key) ? t(key) : t('panel.audit.other');
  if (a.action === 'settings.update' && Array.isArray(d.fields)) {
    const names = d.fields
      .map((f) => {
        const label = `settings.${String(f)}`;
        return known(label) ? t(label) : null;
      })
      .filter((x): x is string => x !== null);
    if (names.length > 0) what = `${what}: ${names.join(', ')}`;
  }
  return { what, target: a.target === null || a.target === undefined ? '' : String(a.target) };
}

export { formatNumber };
