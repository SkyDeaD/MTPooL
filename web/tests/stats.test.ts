// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { t, tp } from '../src/lib/i18n';
import { otherClientsTotal } from '../src/lib/model';
import type { PublicNode } from '../src/lib/types';
import Stats from '../src/public/Stats.svelte';
import { fakeApi, FIXTURE_NOW, settle, setHidden, type FakeApi } from './helpers';

let app: ReturnType<typeof mount> | undefined;
let target: HTMLElement;
let api: FakeApi;

function open() {
  app = mount(Stats, { target, props: { fetchFn: api.fetchFn } });
}

beforeEach(() => {
  vi.useFakeTimers({ now: FIXTURE_NOW });
  setHidden(false);
  target = document.createElement('div');
  document.body.append(target);
  api = fakeApi();
});
afterEach(() => {
  if (app) unmount(app);
  app = undefined;
  target.remove();
  vi.useRealTimers();
});

describe('the owners own users', () => {
  it('a_node_shows_them_apart_with_the_right_word', () => {
    expect(t('clients.other', { n: 41, word: tp('unit.ownClient', 41) })).toBe('ещё 41 свой');
    expect(t('clients.other', { n: 40, word: tp('unit.ownClient', 40) })).toBe('ещё 40 своих');
    expect(t('clients.other', { n: 2, word: tp('unit.ownClient', 2) })).toBe('ещё 2 своих');
  });

  it('only_the_nodes_the_pool_counts_add_to_the_total', () => {
    const nodes = [
      { state: 'healthy', other_clients: 5 },
      { state: 'quarantine', other_clients: 2 },
      { state: 'dead', other_clients: 90 },
      { state: 'healthy' },
    ] as PublicNode[];
    expect(otherClientsTotal(nodes)).toBe(7);
  });

  it('a_pending_node_is_not_on_the_page_and_adds_nothing', () => {
    const nodes = [
      { state: 'healthy', other_clients: 5 },
      { state: 'pending', other_clients: 3065 },
    ] as PublicNode[];
    expect(otherClientsTotal(nodes)).toBe(5);
  });
});

describe('stats page', () => {
  it('shows_pool_groups_and_counters', async () => {
    open();
    await settle();
    const groups = [...target.querySelectorAll('tr.grp')].map((g) => g.textContent ?? '');
    expect(groups).toHaveLength(3);
    expect(groups[0]).toContain(t('stats.group.masters'));
    expect(groups[1]).toContain(t('stats.group.queue'));
    expect(groups[2]).toContain(t('stats.group.quarantine'));
    expect(target.querySelectorAll('tr.go')).toHaveLength(7);
    // Six nodes are healthy among the seven shown.
    expect(target.querySelector('[data-kpi="working"]')?.textContent).toContain('6');
    // The mean of the six known availabilities; the node without a figure is not counted.
    expect(target.querySelector('[data-kpi="availability"]')?.textContent).toContain('97,89');
  });

  it('a_node_out_of_the_queue_has_its_own_group_and_is_not_called_a_quarantine', async () => {
    const list = JSON.parse(readFileSync(join(process.cwd(), 'tests', 'fixtures', 'nodes.json'), 'utf8')) as PublicNode[];
    const outside = { ...list[0], id: 'iota-p2k8v', is_master: false, state: 'quarantine', quarantine_reasons: ['tcp'] };
    api.set('nodes', [...list, outside]);
    open();
    await settle();
    const groups = [...target.querySelectorAll('tr.grp')].map((g) => g.textContent ?? '');
    expect(groups).toHaveLength(4);
    expect(groups[2]).toContain(t('stats.group.quarantine'));
    expect(groups[3]).toContain(t('stats.group.outside'));
    const row = (id: string) => [...target.querySelectorAll('tr.go')].find((r) => r.textContent?.includes(id))!;
    expect(row('theta-m1d7z').textContent).toContain(t('node.state.quarantine'));
    expect(row('iota-p2k8v').textContent).toContain(t('node.state.outside'));
    expect(row('iota-p2k8v').textContent).not.toContain(t('node.state.quarantine'));
  });

  it('a_disputed_node_says_so_instead_of_a_failed_check', async () => {
    const list = JSON.parse(readFileSync(join(process.cwd(), 'tests', 'fixtures', 'nodes.json'), 'utf8')) as PublicNode[];
    const id = list.find((n) => n.state === 'healthy' && !n.is_master)!.id;
    api.set(
      'nodes',
      list.map((n) => (n.id === id ? { ...n, globalping_disputed: true, checks: { ...n.checks, globalping: 'failed' } } : n)),
    );
    open();
    await settle();
    const row = [...target.querySelectorAll('tr.go')].find((r) => r.textContent?.includes(id))!;
    const titles = [...row.querySelectorAll('.chks li')].map((l) => l.getAttribute('title'));
    expect(titles).toContain(t('check.disputed.globalping'));
    expect(titles).not.toContain(t('check.bad.globalping'));
    expect(row.querySelector('.chks li.bad')).toBeNull();
  });

  it('a_node_passing_registration_is_on_the_page_under_its_own_group', async () => {
    const list = JSON.parse(readFileSync(join(process.cwd(), 'tests', 'fixtures', 'nodes.json'), 'utf8')) as PublicNode[];
    const pending = { ...list[0], id: 'kappa-t6r1b', is_master: false, state: 'pending', clients: 0, other_clients: 3065 };
    api.set('nodes', [...list, pending]);
    open();
    await settle();
    const groups = [...target.querySelectorAll('tr.grp')].map((g) => g.textContent ?? '');
    expect(groups[groups.length - 1]).toContain(t('stats.group.pending'));
    const row = [...target.querySelectorAll('tr.go')].find((r) => r.textContent?.includes('kappa-t6r1b'))!;
    expect(row.textContent).toContain(t('node.state.pending'));
    expect(row.textContent).not.toContain(t('node.state.healthy'));
    expect(row.textContent).not.toContain(t('node.role.queue', { n: '?' }));
    // In the pool it is not yet: the counters do not take it.
    expect(target.querySelector('[data-kpi="working"]')?.textContent).toContain('6');
    expect(target.textContent).not.toContain(t('clients.otherPool', { n: 3065, word: tp('unit.ownClient', 3065) }));
  });

  it('a_pool_with_only_a_registering_node_shows_that_node', async () => {
    const list = JSON.parse(readFileSync(join(process.cwd(), 'tests', 'fixtures', 'nodes.json'), 'utf8')) as PublicNode[];
    api.set('nodes', [{ ...list[0], id: 'kappa-t6r1b', is_master: false, state: 'pending' }]);
    open();
    await settle();
    expect(target.textContent).not.toContain(t('stats.empty.title'));
    expect(target.textContent).toContain('kappa-t6r1b');
  });

  it('stats_empty_pool_shows_explanation', async () => {
    api.set('nodes', []);
    api.set('domains', []);
    open();
    await settle();
    const empty = target.querySelector('.empty');
    expect(empty).not.toBeNull();
    expect(empty!.textContent).toContain(t('stats.empty.title'));
    expect(empty!.textContent).toContain(t('stats.empty.text'));
    expect(target.querySelectorAll('tr.go')).toHaveLength(0);
    expect(target.querySelector('.stale')).toBeNull();
  });

  it('api_error_keeps_last_data_and_shows_stale_note', async () => {
    open();
    await settle();
    expect(target.textContent).toContain('alpha-k3m2q');
    expect(target.querySelector('.stale')).toBeNull();

    api.fail(true);
    await vi.advanceTimersByTimeAsync(30_000);
    await settle();
    expect(target.textContent).toContain('alpha-k3m2q');
    expect(target.querySelector('tr.go')).not.toBeNull();
    const note = target.querySelector('.stale');
    expect(note).not.toBeNull();
    expect(note!.textContent).toContain(t('common.stale'));

    // The note goes away with the next good answer.
    api.fail(false);
    await vi.advanceTimersByTimeAsync(30_000);
    await settle();
    expect(target.querySelector('.stale')).toBeNull();
  });

  it('first_load_failure_shows_error_not_empty_pool', async () => {
    api.fail(true);
    open();
    await settle();
    expect(target.querySelector('.empty')).toBeNull();
    expect(target.querySelector('.stale')).not.toBeNull();
  });

  it('table_becomes_list_at_360', async () => {
    open();
    await settle();
    const table = target.querySelector('table.tb.list');
    expect(table).not.toBeNull();
    // Every cell that is not the node name carries its column name for the list layout.
    const row = target.querySelector('tr.go')!;
    const labels = [...row.querySelectorAll('td[data-l]')].map((c) => c.getAttribute('data-l'));
    expect(labels).toContain(t('stats.col.state'));
    expect(labels).toContain(t('stats.col.checks'));
    expect(labels).toContain(t('stats.col.availability'));
    expect(labels).toContain(t('stats.col.clients'));
    // The stylesheet turns it into a list from 720px down, which covers 360.
    const css = readFileSync(join(process.cwd(), 'src', 'styles', 'pages.css'), 'utf8');
    const block = css.slice(css.indexOf('@media (max-width:720px)'));
    expect(block).toContain('.tb.list td[data-l]::before');
    expect(block).toContain('.tb.list thead{display:none}');
  });

  it('rows_link_to_the_node_page', async () => {
    open();
    await settle();
    const link = target.querySelector<HTMLAnchorElement>('tr.go a');
    expect(link?.getAttribute('href')).toBe('/node/?id=alpha-k3m2q');
  });

  it('clients_range_switch_asks_for_that_range', async () => {
    open();
    await settle();
    expect(api.calls.some((c) => c.url === '/api/v1/stats/clients?range=10m')).toBe(true);
    const button = [...target.querySelectorAll<HTMLButtonElement>('.cth .seg button')].find(
      (b) => b.textContent?.trim() === t('clients.range.1h'),
    );
    button!.click();
    await settle();
    expect(api.calls.some((c) => c.url === '/api/v1/stats/clients?range=1h')).toBe(true);
  });
});
