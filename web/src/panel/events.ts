// Journal entries in words. The server sends a kind and a few fields; the panel never shows them raw.
import { formatNumber, formatSpan } from '../lib/dates';
import { sensorName, whyText } from '../lib/model';
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
  node_ip_changed: 'nodes',
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
  node_ip_changed: 'acc',
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

const sentence = (text: string): string => (text ? `${text[0].toUpperCase()}${text.slice(1)}` : text);

interface VoteEntry {
  sensor: string;
  vote: unknown;
}

const votesOf = (votes: unknown): VoteEntry[] => (Array.isArray(votes) ? (votes as VoteEntry[]) : []);

const namesWith = (votes: VoteEntry[], test: (vote: unknown) => boolean): string =>
  votes
    .filter((v) => test(v.vote))
    .map((v) => sensorName(String(v.sensor)))
    .join(', ');

const isBlocked = (vote: unknown) => vote === 'blocked';
const isClear = (vote: unknown) => vote === 'clear';
const isAbstain = (vote: unknown) => !isBlocked(vote) && !isClear(vote);

/** How the sensors of a round voted: only the groups somebody is in. */
function votesText(votes: VoteEntry[]): string {
  const part = (key: Key, names: string) => (names ? t(key, { names }) : '');
  return [
    part('panel.ev.votesBlocked', namesWith(votes, isBlocked)),
    part('panel.ev.votesClear', namesWith(votes, isClear)),
    part('panel.ev.votesAbstain', namesWith(votes, isAbstain)),
  ]
    .filter(Boolean)
    .join('; ');
}

const str = (value: unknown): string => (value === null || value === undefined ? '' : String(value));

function deadText(d: Record<string, unknown>, node: string): { title: string; detail: string } {
  if (d.replaced_by) {
    const by = str(d.replaced_by);
    return {
      title: t('panel.ev.replaced', { node, by }),
      detail: t(d.reason === 'silent' ? 'panel.ev.replacedSilent' : 'panel.ev.replacedSame'),
    };
  }
  if (d.retired) return { title: t('panel.ev.retired', { node }), detail: t('panel.ev.retiredDetail') };
  if (d.reason === 'address_refused') return { title: t('panel.ev.dead', { node }), detail: t('panel.ev.deadRefused') };
  return { title: t('panel.ev.dead', { node }), detail: t('panel.ev.deadSilent') };
}

function srmdText(d: Record<string, unknown>, domain: string): { title: string; detail: string } {
  const key = `panel.ev.srmd.${str(d.action)}`;
  const title = known(key) ? t(key, { domain }) : t('panel.event.srmd_action');
  const detail = d.action === 'fold' && d.target ? t('panel.ev.srmdTarget', { target: str(d.target) }) : '';
  return { title, detail };
}

export function describeEvent(e: EventEntry): Described {
  const d = (e.detail ?? {}) as Record<string, unknown>;
  const node = e.node ?? '';
  const domain = e.domain ?? '';
  let tone: Tone = TONE[e.kind] ?? 'acc';
  let title: string;
  let detail = '';
  switch (e.kind) {
    case 'sensor_round': {
      title = t('panel.ev.round', { node });
      detail = [verdictText(d.verdict), sentence(votesText(votesOf(d.votes)))].filter(Boolean).join('. ');
      tone = d.verdict === 'clean' ? 'ok' : d.verdict === 'confirmed' ? 'bad' : 'warn';
      break;
    }
    case 'master_changed': {
      const added = str(d.added);
      const removed = str(d.removed);
      title = t(added ? 'panel.ev.master' : 'panel.ev.masterGone', { domain });
      detail = added && removed ? t('panel.ev.masterDetail', { node: added, from: removed }) : added || removed;
      break;
    }
    case 'node_registered': {
      title = t('panel.ev.registered', { node });
      const parts = [d.ip ? t('panel.ev.address', { ip: str(d.ip) }) : ''];
      if (d.reverify) parts.push(t('panel.ev.registeredReverify'));
      if (d.replaced) parts.push(t('panel.ev.registeredReplaced', { node: str(d.replaced) }));
      detail = parts.filter(Boolean).join('. ');
      break;
    }
    case 'node_ip_changed':
      title = t('panel.ev.moved', { node });
      detail = t('panel.ev.movedDetail', { from: str(d.from), to: str(d.to) });
      break;
    case 'node_quarantined': {
      title = t('panel.ev.quarantined', { node });
      const reasons = Array.isArray(d.reason) ? d.reason.map(String) : [];
      detail = sentence(whyText(reasons));
      break;
    }
    case 'node_recovered':
      title = t('panel.ev.recovered', { node });
      break;
    case 'node_banned': {
      title = t('panel.ev.banned', { node });
      const names = namesWith(votesOf(d.votes), isBlocked);
      detail = [names ? t('panel.ev.bannedDetail', { names }) : '', t('panel.ev.bannedGone')].filter(Boolean).join('. ');
      break;
    }
    case 'node_reverify_failed':
      title = t('panel.ev.reverify', { node });
      detail = typeof d.retry_after_sec === 'number' ? t('panel.ev.reverifyDetail', { span: formatSpan(d.retry_after_sec) }) : '';
      break;
    case 'ban_lifted':
      title = t('panel.ev.lifted', { node });
      detail = t(d.admin ? 'panel.ev.liftedAdmin' : 'panel.ev.liftedCheck');
      break;
    case 'node_dead':
      ({ title, detail } = deadText(d, node));
      break;
    case 'node_pruned':
      title = t('panel.ev.pruned', { node });
      detail = t('panel.ev.prunedDetail');
      break;
    case 'dns_applied':
      title = t('panel.ev.dns', { domain });
      if (Array.isArray(d.addresses)) detail = t('panel.ev.dnsAddresses', { list: d.addresses.map(String).join(', ') });
      else if (d.target) detail = t('panel.ev.dnsTarget', { target: str(d.target) });
      break;
    case 'dns_failed': {
      title = t('panel.ev.dnsFailed', { domain });
      const retry = typeof d.retry_in_sec === 'number' ? t('panel.ev.dnsRetry', { span: formatSpan(d.retry_in_sec) }) : '';
      detail = [str(d.error), retry].filter(Boolean).join('. ');
      break;
    }
    case 'srmd_action':
      ({ title, detail } = srmdText(d, domain));
      break;
    case 'clock_skew':
      title = t('panel.ev.skew', { node });
      detail = typeof d.skew_sec === 'number' ? t('panel.ev.skewDetail', { n: d.skew_sec }) : '';
      break;
    case 'store_error':
      title = t('panel.event.store_error');
      detail = str(d.error);
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
