// @vitest-environment jsdom
import { dayOf } from '../src/lib/dates';
import { FIELDS, parse, toText } from '../src/panel/settings-fields';
import { panelMessages } from '../src/strings.panel.ru';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { t } from '../src/panel/i18n';
import App from '../src/panel/App.svelte';
import { MOCK_TOKEN } from '../scripts/admin-mock';
import { FIXTURE_NOW, settle, setHidden } from './helpers';
import { fakeAdmin, FakeEventSource, type FakeAdmin } from './panel-helpers';

let app: ReturnType<typeof mount> | undefined;
let target: HTMLElement;
let api: FakeAdmin;

function start(hash = ''): void {
  window.location.hash = hash;
  app = mount(App, {
    target,
    props: { fetchFn: api.fetchFn, eventSource: FakeEventSource as unknown as typeof EventSource },
  });
}

beforeEach(() => {
  vi.useFakeTimers({ now: FIXTURE_NOW });
  setHidden(false);
  FakeEventSource.reset();
  api = fakeAdmin();
  target = document.createElement('div');
  document.body.append(target);
});
afterEach(() => {
  if (app) unmount(app);
  app = undefined;
  target.remove();
  document.body.replaceChildren();
  vi.useRealTimers();
});

const text = () => document.body.textContent ?? '';

/** One polling interval, exactly: `settle` repeats its step, which would be four rounds here. */
async function pollOnce(): Promise<void> {
  await vi.advanceTimersByTimeAsync(30_000);
  flushSync();
  await vi.advanceTimersByTimeAsync(5);
  flushSync();
}

function type(input: HTMLInputElement | HTMLTextAreaElement, value: string): void {
  input.value = value;
  input.dispatchEvent(new Event('input', { bubbles: true }));
  flushSync();
}

async function click(el: Element | null | undefined): Promise<void> {
  expect(el, 'element to click').toBeTruthy();
  (el as HTMLElement).click();
  await settle();
}

function button(label: string, root: ParentNode = document.body): HTMLButtonElement | undefined {
  return [...root.querySelectorAll('button')].find((b) => b.textContent?.trim() === label);
}

async function login(token = MOCK_TOKEN): Promise<void> {
  await settle();
  const input = document.querySelector<HTMLInputElement>('input[name="token"]');
  expect(input).toBeTruthy();
  type(input!, token);
  await click(button(t('panel.login.submit')));
  await settle(50);
}

describe('login and session', () => {
  it('login_with_token_then_session', async () => {
    start('#/overview');
    await login();
    const post = api.calls.find((c) => c.method === 'POST' && c.path === '/admin/session');
    expect(post?.body).toEqual({ token: MOCK_TOKEN });
    expect(document.querySelector('h1')?.textContent).toBe(t('panel.overview.title'));
    // The token is neither in the address nor in any storage.
    expect(window.location.href).not.toContain(MOCK_TOKEN);
    expect(JSON.stringify({ ...localStorage })).not.toContain(MOCK_TOKEN);
    expect(JSON.stringify({ ...sessionStorage })).not.toContain(MOCK_TOKEN);
  });

  it('wrong_token_shows_error', async () => {
    start('#/overview');
    await login('nope');
    expect(text()).toContain(t('panel.login.error'));
    expect(document.querySelector('h1')?.textContent).toBe(t('panel.login.title'));
    expect(api.calls.some((c) => c.path === '/admin/nodes')).toBe(false);
  });

  it('session_expired_returns_to_login_keeps_route', async () => {
    start('#/nodes/alpha-k3m2q');
    await login();
    expect(document.querySelector('h1')?.textContent).toContain('alpha-k3m2q');
    api.expire();
    await pollOnce();
    expect(document.querySelector('h1')?.textContent).toBe(t('panel.login.title'));
    expect(text()).toContain(t('panel.login.expired'));
    expect(window.location.hash).toBe('#/nodes/alpha-k3m2q');
    await login();
    expect(document.querySelector('h1')?.textContent).toContain('alpha-k3m2q');
  });
});

describe('settings', () => {
  async function openSettings(): Promise<void> {
    start('#/settings');
    await login();
    await settle(50);
  }

  const key = (k: string, root: ParentNode = document) => root.querySelector<HTMLInputElement>(`[data-key="${k}"]`)!;
  const formOf = (k: string) => key(k).closest('form') as HTMLFormElement;

  it('settings_human_labels_seconds_per_form_save_button', async () => {
    await openSettings();
    const forms = [...document.querySelectorAll<HTMLFormElement>('form.settings')];
    expect(forms.length).toBeGreaterThan(4);
    // Each group has its own save button.
    for (const f of forms) expect(f.querySelectorAll('button[type="submit"]')).toHaveLength(1);
    // Every setting the server has is on the page, by its dotted name (the SRMD ones are on its page).
    const shown = new Set([...document.querySelectorAll<HTMLElement>('[data-key]')].map((e) => e.dataset.key));
    const flat = (obj: Record<string, unknown>, prefix = ''): string[] =>
      Object.entries(obj).flatMap(([k, v]) =>
        k === 'secrets' ? [] : v && typeof v === 'object' && !Array.isArray(v) ? flat(v as Record<string, unknown>, `${prefix}${k}.`) : [`${prefix}${k}`],
      );
    for (const k of flat(api.mock.state.settings).filter((x) => !x.startsWith('srmd.'))) expect(shown, `field ${k}`).toContain(k);
    const hbForm = formOf('heartbeat_interval_sec');
    expect(hbForm.textContent).toContain(t('settings.heartbeat_interval_sec'));
    expect(hbForm.textContent).not.toContain('heartbeat_interval_sec');
    expect(document.body.textContent).not.toContain('globalping_token');
    expect(key('heartbeat_interval_sec').value).toBe('15');
    expect(key('heartbeat_interval_sec').closest('.f')?.textContent).toContain(t('settings.unit.sec'));
    // Rotation is shown in minutes.
    expect(key('rotation_ttl_sec').closest('.f')?.textContent).toContain(t('settings.unit.min'));
  });

  it('settings_unsaved_warning', async () => {
    await openSettings();
    const form = formOf('rotation_ttl_sec');
    expect(form.textContent).not.toContain(t('settings.unsaved'));
    expect(form.querySelector<HTMLButtonElement>('button[type="submit"]')!.disabled).toBe(true);
    type(key('rotation_ttl_sec'), '40');
    expect(form.textContent).toContain(t('settings.unsaved'));
    // Another group stays calm.
    expect(formOf('heartbeat_interval_sec').textContent).not.toContain(t('settings.unsaved'));
    const leave = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(leave);
    expect(leave.defaultPrevented).toBe(true);

    form.requestSubmit();
    await settle(50);
    const patch = api.calls.find((c) => c.method === 'PATCH' && c.path === '/admin/settings');
    // Only this group's fields are sent; minutes are written as seconds.
    expect(Object.keys(patch?.body as object).sort()).toEqual(['domains', 'masters_per_domain', 'rotation_ttl_sec'].filter((k) => k in (patch?.body as object)).sort());
    expect((patch?.body as Record<string, unknown>).rotation_ttl_sec).toBe(2400);
    expect(patch?.headers['x-csrf-token']).toBe('csrf-mock');
    expect(text()).toContain(t('settings.saved'));
    expect(form.textContent).not.toContain(t('settings.unsaved'));
    const after = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(after);
    expect(after.defaultPrevented).toBe(false);
  });

  it('settings_dirty_form_asks_before_leaving_the_page_inside_the_panel', async () => {
    await openSettings();
    type(key('rotation_ttl_sec'), '40');
    window.location.hash = '#/nodes';
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    await settle(30);
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain(t('panel.leave.title'));
    await click(button(t('panel.leave.keep'), document.querySelector('[role="dialog"]')!));
    expect(window.location.hash).toBe('#/settings');
  });

  it('settings_invalid_value_blocks_the_save', async () => {
    await openSettings();
    const form = formOf('heartbeat_interval_sec');
    type(key('heartbeat_interval_sec'), '0');
    form.requestSubmit();
    await settle(50);
    expect(api.writes()).not.toContain('PATCH /admin/settings');
    expect(form.textContent).toContain(t('settings.error.int', { min: 1, max: 86400 }));
  });

  it('secret_fields_write_only', async () => {
    await openSettings();
    const page = document.querySelector('.view') as HTMLElement;
    // The server says only whether a secret is set; no input holds a value.
    expect(page.textContent).toContain(t('settings.secret.set'));
    expect(page.textContent).toContain(t('settings.secret.unset'));
    expect(page.querySelectorAll('input[type="password"]')).toHaveLength(0);
    const replace = [...page.querySelectorAll('button')].filter((b) => b.textContent?.trim() === t('settings.secret.replace'));
    expect(replace.length).toBe(3);
    await click(replace[0]);
    const dialog = document.querySelector('[role="dialog"]');
    expect(dialog?.textContent).toContain(t('settings.secret.title', { label: t('settings.secret.cloudflare_api_token') }));
    const field = dialog!.querySelector<HTMLInputElement>('input')!;
    expect(field.value).toBe('');
    type(field, 'cf-secret-value');
    await click(button(t('common.save'), dialog!));
    await settle(50);
    const patch = api.calls.find((c) => c.method === 'PATCH');
    expect(patch?.body).toEqual({ secrets: { cloudflare_api_token: 'cf-secret-value' } });
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    // The value is gone from the page for good.
    expect(document.body.outerHTML).not.toContain('cf-secret-value');
    for (const input of document.querySelectorAll('input')) expect(input.value).not.toContain('cf-secret-value');
  });
});

describe('nodes list and search', () => {
  const rowsOf = () => [...document.querySelectorAll<HTMLElement>('tbody tr.go a.nm')].map((a) => a.textContent);
  const chip = (name: string) => [...document.querySelectorAll<HTMLButtonElement>('.chips .chip')].find((c) => c.textContent?.includes(name))!;

  it('nodes_chips_filter_and_count', async () => {
    start('#/nodes');
    await login();
    await settle(50);
    const all = rowsOf();
    expect(all).toContain('alpha-k3m2q');
    expect(chip(t('panel.nodes.filter.all')).getAttribute('aria-pressed')).toBe('true');
    await click(chip(t('panel.nodes.filter.master')));
    expect(chip(t('panel.nodes.filter.master')).getAttribute('aria-pressed')).toBe('true');
    expect(rowsOf().slice(0, 3)).toEqual(['alpha-k3m2q', 'beta-r8t4w', 'gamma-x2n9d']);
    expect(rowsOf()).not.toContain('epsilon-h4j6s');
    expect(chip(t('panel.nodes.filter.master')).querySelector('b')?.textContent).toBe('3');
    await click(chip(t('panel.nodes.filter.prob')));
    expect(rowsOf()[0]).toBe('theta-m1d7z');
    expect(rowsOf()).not.toContain('alpha-k3m2q');
  });

  it('nodes_sort_by_clients_and_search_text', async () => {
    start('#/nodes');
    await login();
    await settle(50);
    await click(chip(t('panel.nodes.filter.queue')));
    const before = rowsOf();
    await click(button(t('stats.col.clients')));
    const sorted = rowsOf();
    expect(new Set(sorted)).toEqual(new Set(before));
    expect(sorted[0]).toBe('epsilon-h4j6s');
    expect(sorted.indexOf('zeta-w9b3y')).toBeLessThan(sorted.indexOf('eta-c5f8l'));
    await click(chip(t('panel.nodes.filter.all')));
    type(document.querySelector<HTMLInputElement>('.th input[type="search"]')!, 'gamma');
    expect(rowsOf()).toEqual(['gamma-x2n9d']);
  });

  it('global_search_finds_nodes_domains_and_sections', async () => {
    start('#/overview');
    await login();
    await settle(50);
    const input = document.querySelector<HTMLInputElement>('[role="search"] input')!;
    input.focus();
    input.dispatchEvent(new Event('focus'));
    await settle(50);
    const hits = () => [...document.querySelectorAll<HTMLAnchorElement>('.res a')].map((a) => a.textContent ?? '');
    type(input, 'alpha');
    expect(hits().some((h) => h.includes('alpha-k3m2q') && h.includes(t('panel.search.node')))).toBe(true);
    type(input, 'журнал');
    expect(hits().some((h) => h.includes(t('panel.nav.journal')) && h.includes(t('panel.search.section')))).toBe(true);
    const domain = ((await (await api.fetchFn('/api/v1/domains')).json()) as { domain: string }[])[0].domain;
    type(input, domain);
    await settle(50);
    expect(hits().some((h) => h.includes(domain) && h.includes(t('panel.search.domain')))).toBe(true);
  });
});

describe('tokens', () => {
  it('new_token_shown_once_with_copy', async () => {
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
    start('#/owners');
    await login();
    await click(button(t('panel.tokens.issue')));
    const dialog = document.querySelector('[role="dialog"]')!;
    expect(dialog).toBeTruthy();
    await click(button(t('panel.tokens.create'), dialog));
    const post = api.calls.find((c) => c.method === 'POST' && c.path === '/admin/tokens');
    expect(post?.body).toMatchObject({ kind: 'enroll', owner_id: 1 });
    const shown = document.querySelector('[role="dialog"] pre.code')?.textContent ?? '';
    expect(shown).toMatch(/^mtp_enroll_secret/);
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain(t('panel.tokens.once'));
    await click(button(t('panel.tokens.copy')));
    expect(writeText).toHaveBeenCalledWith(shown);
    await click(button(t('panel.tokens.saved')));
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    // The list has only the prefix: the value is not shown a second time.
    expect(document.body.textContent).not.toContain(shown);
    expect(document.body.textContent).toContain('mtp_enr_');
  });
});

describe('node page', () => {
  it('node_page_shows_votes_by_round_and_provider_hint', async () => {
    start('#/nodes/theta-m1d7z');
    await login();
    const rounds = document.querySelectorAll('table.rounds tbody tr');
    expect(rounds).toHaveLength(3);
    const head = [...document.querySelectorAll('table.rounds thead th')].map((th) => th.textContent?.trim());
    expect(head).toEqual([
      t('panel.rounds.when'),
      t('panel.rounds.verdict'),
      t('sensor.globalping'),
      t('sensor.cheburcheck'),
      t('sensor.redflag'),
    ]);
    // A sensor that did not answer says why, in words.
    expect(rounds[1].textContent).toContain(t('vote.reason.timeout'));
    expect(rounds[0].textContent).toContain(t('vote.blocked'));
    expect(rounds[2].textContent).toContain(t('vote.clear'));
    // Probe cities show only where the evidence has them.
    expect(rounds[0].textContent).toContain('Москва');
    expect(rounds[1].textContent).not.toContain('Москва');
    // The hint about the weakest vote: one "blocked" is not enough for a ban.
    expect(text()).toContain(t('panel.node.hint'));
  });

  it('banned_node_has_unban_and_votes', async () => {
    start('#/nodes/iota-p6q1c');
    await login();
    expect(button(t('panel.node.unban'))).toBeTruthy();
    expect(button(t('panel.node.recheck'))).toBeUndefined();
  });
});

describe('live data', () => {
  it('stream_reconnect_and_fallback_to_poll', async () => {
    start('#/nodes');
    await login();
    const es = FakeEventSource.last!;
    expect(es.url).toBe('/api/v1/admin/stream');
    es.open();
    await settle();
    expect(text()).toContain(t('panel.live.on'));
    const base = api.count('/admin/nodes');
    expect(base).toBe(1);

    // An event refreshes what it touches at once.
    es.emit('node_banned', { at: '2026-11-04T12:00:00Z', kind: 'node_banned', node: 'alpha-k3m2q' });
    await settle(50);
    expect(api.count('/admin/nodes')).toBe(base + 1);

    // The stream drops: the page says so and keeps polling every 30 seconds.
    es.fail();
    await settle();
    expect(text()).toContain(t('panel.live.off'));
    await pollOnce();
    expect(api.count('/admin/nodes')).toBe(base + 2);

    // The browser gives up on the connection: the panel opens a new one after a pause.
    es.fail(true);
    await settle(6_000);
    expect(FakeEventSource.all.length).toBe(2);
    // Events missed while it was down are not replayed, so a reconnect reads everything again.
    FakeEventSource.last!.open();
    await settle(50);
    expect(api.count('/admin/nodes')).toBeGreaterThan(base + 2);
    expect(text()).toContain(t('panel.live.on'));
  });

  it('no_stream_support_means_polling_only', async () => {
    window.location.hash = '#/nodes';
    app = mount(App, { target, props: { fetchFn: api.fetchFn, eventSource: null } });
    await login();
    expect(text()).toContain(t('panel.live.off'));
    await pollOnce();
    expect(api.count('/admin/nodes')).toBe(2);
  });
});

describe('cloudflare', () => {
  it('cloudflare_error_banner', async () => {
    api.mock.state.status.dns = {
      ...api.mock.state.status.dns,
      last_error: 'zone not found',
      last_failed_at: '2026-11-04T11:58:00Z',
      failed_24h: 2,
      unsynced_domains: 1,
    };
    start('#/masters');
    await login();
    const banner = document.querySelector('[role="alert"]');
    expect(banner?.textContent).toContain(t('panel.cf.errorTitle'));
    expect(banner?.textContent).toContain('zone not found');
    expect(button(t('panel.masters.push'))).toBeTruthy();
  });

  it('no_banner_when_dns_is_fine', async () => {
    start('#/masters');
    await login();
    expect(document.querySelector('[role="alert"]')).toBeNull();
  });
});

describe('destructive actions', () => {
  it('destructive_actions_confirm', async () => {
    start('#/nodes');
    await login();
    // A node leaves the pool only after a confirmation.
    const menu = document.querySelector<HTMLButtonElement>('button[aria-label="' + t('panel.nodes.actions') + '"]');
    await click(menu);
    await click(button(t('panel.nodes.retire')));
    const dialog = document.querySelector('[role="dialog"]')!;
    expect(dialog.textContent).toContain(t('panel.retire.text'));
    expect(api.writes().some((w) => w.includes('/retire'))).toBe(false);
    await click(button(t('panel.retire.keep'), dialog));
    expect(api.writes().some((w) => w.includes('/retire'))).toBe(false);
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    await click(document.querySelector<HTMLButtonElement>('button[aria-label="' + t('panel.nodes.actions') + '"]'));
    await click(button(t('panel.nodes.retire')));
    await click(button(t('panel.retire.confirm'), document.querySelector('[role="dialog"]')!));
    expect(api.writes().filter((w) => w.endsWith('/retire'))).toHaveLength(1);
  });

  it('token_revoke_and_rollout_cancel_confirm', async () => {
    start('#/owners');
    await login();
    await click(document.querySelector<HTMLButtonElement>('button[data-revoke]'));
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain(t('panel.revoke.text'));
    expect(api.writes().some((w) => w.startsWith('DELETE /admin/tokens'))).toBe(false);
    await click(button(t('panel.revoke.keep'), document.querySelector('[role="dialog"]')!));
    await click(document.querySelector<HTMLButtonElement>('button[data-revoke]'));
    await click(button(t('panel.revoke.confirm'), document.querySelector('[role="dialog"]')!));
    expect(api.writes().filter((w) => w.startsWith('DELETE /admin/tokens'))).toHaveLength(1);

    window.location.hash = '#/updates';
    window.dispatchEvent(new HashChangeEvent('hashchange'));
    await settle(50);
    await click(button(t('panel.updates.cancel')));
    expect(api.writes().some((w) => w.endsWith('/cancel'))).toBe(false);
    await click(button(t('panel.updates.cancelConfirm'), document.querySelector('[role="dialog"]')!));
    expect(api.writes().filter((w) => w.endsWith('/cancel'))).toHaveLength(1);
  });
});

describe('settings limits follow the server', () => {
  const field = (key: string) => FIELDS.get(key)!;

  it('the domain list may be empty, SRMD then fills it from the base name', () => {
    expect(parse(field('domains'), '  ')).toEqual({ ok: true, value: [] });
    expect(parse(field('domains'), 'a.example\nb.example')).toEqual({ ok: true, value: ['a.example', 'b.example'] });
  });

  it('masters_per_domain_stops_at_8', () => {
    expect(parse(field('masters_per_domain'), '8').ok).toBe(true);
    expect(parse(field('masters_per_domain'), '9').ok).toBe(false);
  });

  it('rotation_zero_turns_it_off_and_12_hours_is_the_top', () => {
    expect(parse(field('rotation_ttl_sec'), '0')).toEqual({ ok: true, value: 0 });
    expect(parse(field('rotation_ttl_sec'), '720')).toEqual({ ok: true, value: 43200 });
    expect(parse(field('rotation_ttl_sec'), '721').ok).toBe(false);
  });

  it('latch_thresholds_stop_at_255', () => {
    expect(parse(field('latch_fail_threshold'), '256').ok).toBe(false);
  });

  it('an_empty_base_domain_means_the_first_listed_domain', () => {
    expect(parse(field('srmd.base_domain'), '  ')).toEqual({ ok: true, value: null });
    expect(toText(field('srmd.base_domain'), null)).toBe('');
  });

  it('the_block_threshold_reads_in_percent', () => {
    expect(toText(field('globalping.success_threshold'), 0.5)).toBe('50');
    expect(parse(field('globalping.success_threshold'), '50')).toEqual({ ok: true, value: 0.5 });
    expect(parse(field('globalping.success_threshold'), '0').ok).toBe(false);
    expect(parse(field('globalping.success_threshold'), '101').ok).toBe(false);
  });

  it('every_setting_has_a_hint_and_every_tab_panel_hides_with_css', () => {
    for (const key of FIELDS.keys()) expect(`settings.h.${key}` in panelMessages, key).toBe(true);
    const css = readFileSync(join(process.cwd(), 'src/styles/base.css'), 'utf8');
    expect(css).toMatch(/\[hidden\]\s*\{\s*display:\s*none\s*!important/);
  });
});

describe('journal days', () => {
  it('a_day_turns_at_midnight_moscow_time_whatever_the_browser_zone', () => {
    const lateEvening = Date.parse('2026-11-03T19:13:00Z'); // 22:13 in Moscow
    const afterMidnight = Date.parse('2026-11-03T21:30:00Z'); // 00:30 in Moscow
    expect(dayOf(lateEvening)).not.toBe(dayOf(afterMidnight));
    expect(dayOf(lateEvening)).toBe('3 нояб.');
  });
});
