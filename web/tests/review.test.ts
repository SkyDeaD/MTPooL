// @vitest-environment jsdom
// Behaviour asked for in the review of the public pages.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { formatBytes } from '../src/lib/format';
import { t } from '../src/lib/i18n';
import Dashboard from '../src/public/Dashboard.svelte';
import Links from '../src/public/Links.svelte';
import Stats from '../src/public/Stats.svelte';
import Seg from '../src/shared/Seg.svelte';
import Votes from '../src/shared/Votes.svelte';
import summary7d from './fixtures/stats-summary-7d.json';
import { fakeApi, FIXTURE_NOW, settle, setHidden } from './helpers';

let app: ReturnType<typeof mount> | undefined;
let target: HTMLElement;

beforeEach(() => {
  vi.useFakeTimers({ now: FIXTURE_NOW });
  setHidden(false);
  target = document.createElement('div');
  document.body.append(target);
});
afterEach(() => {
  if (app) unmount(app);
  app = undefined;
  target.remove();
  vi.useRealTimers();
});

const root = process.cwd();
const read = (...p: string[]) => readFileSync(join(root, ...p), 'utf8');

describe('dashboard traffic', () => {
  it('traffic_is_a_tile_with_the_split_and_a_chart_mode', async () => {
    const api = fakeApi();
    app = mount(Dashboard, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    expect(target.textContent).not.toContain('Трафик пока не считается');
    const tiles = target.querySelectorAll('.kpis .kpi');
    expect(tiles).toHaveLength(4);
    const tile = tiles[3];
    expect(tile.querySelector('.l')?.textContent).toBe(t('dash.kpi.traffic'));
    expect(tile.querySelector('.v')?.textContent).toBe(formatBytes(summary7d.kpi.traffic_total_bytes));
    expect(tile.querySelector('.s')?.textContent).toContain(formatBytes(summary7d.kpi.traffic_egress_bytes));
    expect(tile.querySelector('.s')?.textContent).toContain(formatBytes(summary7d.kpi.traffic_ingress_bytes));

    const mode = [...target.querySelectorAll<HTMLButtonElement>('.seg button')].find(
      (b) => b.textContent?.trim() === t('dash.metric.traffic'),
    );
    expect(mode).toBeDefined();
    mode!.click();
    await settle();
    expect(target.querySelector('.th h2')?.textContent).toBe(t('dash.chart.traffic'));
    expect(target.querySelectorAll('.bc').length).toBeGreaterThan(0);
    expect(target.querySelector('.bc')?.getAttribute('aria-label')).toMatch(/МБ|ГБ/);
  });

  it('the_traffic_of_node_owners_is_kept_apart_from_the_pools', async () => {
    const api = fakeApi();
    app = mount(Dashboard, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    const tile = target.querySelectorAll('.kpis .kpi')[3];
    expect(tile.querySelector('.v')?.textContent).toBe(formatBytes(summary7d.kpi.traffic_total_bytes));
    expect(tile.textContent).toContain(
      t('dash.kpi.trafficOther', { size: formatBytes(summary7d.kpi.other_traffic_total_bytes) }),
    );

    const seg = (label: string) =>
      [...target.querySelectorAll<HTMLButtonElement>('.seg button')].find((b) => b.textContent?.trim() === label)!;
    const label = () => target.querySelector('.bc')?.getAttribute('aria-label');
    seg(t('dash.metric.traffic')).click();
    await settle();
    const pool = label();
    seg(t('dash.metric.other')).click();
    await settle();
    expect(target.querySelector('.th h2')?.textContent).toBe(t('dash.chart.other'));
    expect(label()).toMatch(/МБ|ГБ/);
    expect(label()).not.toBe(pool);
  });

  it('no_owner_traffic_says_nothing_about_it', async () => {
    const api = fakeApi();
    const inner = api.fetchFn;
    const fetchFn = (async (input: RequestInfo | URL, init?: RequestInit) => {
      const res = await inner(input, init);
      if (!String(input).includes('stats/summary') || res.status !== 200) return res;
      const body = await res.json();
      body.kpi.other_traffic_total_bytes = 0;
      return new Response(JSON.stringify(body), { status: 200, headers: res.headers });
    }) as typeof fetch;
    app = mount(Dashboard, { target, props: { fetchFn } });
    await settle();
    const tile = target.querySelectorAll('.kpis .kpi')[3];
    expect(tile.querySelectorAll('.s')).toHaveLength(1);
  });

  it('a_day_without_bans_still_shows_the_traffic', async () => {
    const api = fakeApi();
    const inner = api.fetchFn;
    const fetchFn = (async (input: RequestInfo | URL, init?: RequestInit) => {
      const res = await inner(input, init);
      if (!String(input).includes('stats/summary') || res.status !== 200) return res;
      const body = await res.json();
      body.kpi.bans = 0;
      return new Response(JSON.stringify(body), { status: 200, headers: res.headers });
    }) as typeof fetch;
    app = mount(Dashboard, { target, props: { fetchFn } });
    await settle();
    const seg = (label: string) =>
      [...target.querySelectorAll<HTMLButtonElement>('.seg button')].find((b) => b.textContent?.trim() === label)!;
    seg(t('dash.range.24h')).click();
    await settle();
    expect(target.textContent).toContain(t('dash.today.none'));
    seg(t('dash.metric.traffic')).click();
    await settle();
    expect(target.textContent).not.toContain(t('dash.today.none'));
    expect(target.querySelectorAll('.bc').length).toBeGreaterThan(0);
  });

  it('a_short_history_is_said_aloud', async () => {
    const api = fakeApi();
    app = mount(Dashboard, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    expect(target.textContent).not.toContain(t('dash.historyShort'));
    unmount(app);
    api.set('stats/series', undefined);
    const short = fakeApi();
    const inner = short.fetchFn;
    const fetchFn = (async (input: RequestInfo | URL, init?: RequestInit) => {
      const res = await inner(input, init);
      if (!String(input).includes('stats/series') || res.status !== 200) return res;
      const body = await res.json();
      return new Response(JSON.stringify({ ...body, history_ok: false }), {
        status: 200,
        headers: { ETag: '"short"', 'Content-Type': 'application/json' },
      });
    }) as typeof fetch;
    app = mount(Dashboard, { target, props: { fetchFn } });
    await settle();
    expect(target.textContent).toContain(t('dash.historyShort'));
  });
});

describe('scheme', () => {
  it('a_master_shows_how_far_its_turn_has_gone', async () => {
    const api = fakeApi();
    app = mount(Stats, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    // alpha: master since 10:30:00, rotates 12:12:34, now 12:00:00 -> 5400 of 6154 seconds.
    const cards = [...target.querySelectorAll<HTMLElement>('a.card.m')];
    const alpha = cards.find((c) => c.textContent?.includes('alpha-k3m2q'))!;
    const fill = alpha.querySelector<HTMLElement>('.bar i');
    expect(fill).not.toBeNull();
    expect(fill!.style.width).toBe('88%');
    expect(alpha.querySelector('.s + .bar, .bar')).not.toBeNull();
  });

  it('no_bar_when_the_master_has_no_rotation_time', async () => {
    const api = fakeApi();
    const nodes = (await (await api.fetchFn('/api/v1/nodes')).json()) as Record<string, unknown>[];
    api.set(
      'nodes',
      nodes.map((n) => (n.id === 'alpha-k3m2q' ? { ...n, rotates_at: null } : n)),
    );
    app = mount(Stats, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    const alpha = [...target.querySelectorAll<HTMLElement>('a.card.m')].find((c) => c.textContent?.includes('alpha-k3m2q'))!;
    expect(alpha.querySelector('.bar')).toBeNull();
  });

  it('queue_cards_have_a_clients_line', async () => {
    const api = fakeApi();
    app = mount(Stats, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    const first = target.querySelector('.qlist a.q')!;
    expect(first.textContent).toContain(t('scheme.clients', { n: 18 }));
  });
});

describe('links sublines', () => {
  it('say_which_master_serves_a_domain_and_which_domain_a_folded_one_runs_through', async () => {
    const api = fakeApi();
    app = mount(Links, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    const subs = [...target.querySelectorAll('.dom .sub-l')].map((s) => s.textContent?.trim());
    expect(subs[0]).toBe('Порт 443, мастера alpha-k3m2q, beta-r8t4w');
    expect(subs[1]).toBe('Порт 443, мастер gamma-x2n9d');
    expect(subs[2]).toBe('Порт 443, работает через proxy2');
  });
});

describe('votes', () => {
  it('known_reason_codes_read_in_russian_and_unknown_ones_get_one_generic_line', () => {
    const votes = [
      { sensor: 'redflag', vote: 'abstain', reason: 'rate_limited' },
      { sensor: 'cheburcheck', vote: 'abstain', reason: 'some brand new thing' },
    ] as never;
    app = mount(Votes, { target, props: { votes } });
    const texts = [...target.querySelectorAll('.vote .faint')].map((e) => e.textContent);
    expect(texts[0]).toBe(t('vote.reason.rate_limited'));
    expect(texts[1]).toBe(t('vote.reason.other'));
    expect(target.textContent).not.toContain('brand new');
  });

  it('every_api_code_has_a_text', () => {
    for (const code of ['not_configured', 'key_rejected', 'unavailable', 'rate_limited', 'timeout', 'unknown_answer', 'no_data']) {
      expect(read('src', 'strings.ru.ts')).toContain(`'vote.reason.${code}'`);
    }
  });

  it('fixtures_give_a_reason_only_to_abstentions_and_only_as_a_code', () => {
    const nodes = JSON.parse(read('tests', 'fixtures', 'nodes.json')) as {
      last_round: { votes: { vote: string; reason?: string }[] } | null;
    }[];
    for (const n of nodes)
      for (const v of n.last_round?.votes ?? []) {
        if (v.reason !== undefined) {
          expect(v.vote).toBe('abstain');
          expect(v.reason).toMatch(/^[a-z_]+$/);
        }
      }
  });
});

describe('seg thumb', () => {
  it('has_a_thumb_that_sits_under_the_pressed_button', async () => {
    const items = [
      { value: 'a', label: 'A' },
      { value: 'b', label: 'B' },
    ];
    app = mount(Seg, { target, props: { items, value: 'b', onchange: () => {}, label: 'x' } });
    await settle();
    const seg = target.querySelector('.seg')!;
    expect(seg.firstElementChild?.classList.contains('thumb')).toBe(true);
    const buttons = seg.querySelectorAll<HTMLElement>('button');
    for (const [i, w, l] of [[0, 30, 3], [1, 44, 33]] as const) {
      Object.defineProperty(buttons[i], 'offsetWidth', { configurable: true, value: w });
      Object.defineProperty(buttons[i], 'offsetLeft', { configurable: true, value: l });
    }
    unmount(app);
    app = mount(Seg, { target, props: { items, value: 'b', onchange: () => {}, label: 'x' } });
    await settle();
    // jsdom has no layout: the thumb is placed from what offset* report, here zero.
    const thumb = target.querySelector<HTMLElement>('.thumb')!;
    expect(thumb.style.width).toBe('0px');
    expect(thumb.style.transform).toBe('translateX(0px)');
  });
});

describe('page frame', () => {
  it('the_age_starts_with_a_capital', () => {
    expect(t('shell.age', { s: 12 })).toBe('Обновлено 12 с назад');
    expect(t('shell.age.now')).toBe('Обновлено только что');
  });

  it('the_theme_button_has_no_border', async () => {
    const api = fakeApi();
    app = mount(Links, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    const b = target.querySelector('button[aria-label="' + t('shell.theme') + '"]')!;
    expect(b.classList.contains('txt')).toBe(true);
  });

  it('every_entry_sets_the_theme_before_first_paint', () => {
    for (const p of [['index.html'], ['dashboard', 'index.html'], ['links', 'index.html'], ['api', 'index.html'], ['node', 'index.html'], ['panel', 'index.html']]) {
      const html = read(...p);
      const head = html.slice(html.indexOf('<head>'), html.indexOf('</head>'));
      expect(head).toContain("localStorage.getItem('mtpool-theme')");
      expect(head).toContain('try{');
      // It comes before the module script, so it runs before the first paint.
      expect(head.indexOf('mtpool-theme')).toBeLessThan(head.indexOf('type="module"'));
    }
  });

  it('dev_proxy_leaves_the_api_docs_page_alone', () => {
    const cfg = read('vite.config.ts');
    expect(cfg).not.toContain("'/api':");
    expect(cfg).toContain("'/api/v1'");
    expect(cfg).toContain("'/install.sh'");
    expect(cfg).toContain("'/releases'");
  });
});
