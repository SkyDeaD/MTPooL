// @vitest-environment jsdom
import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { t } from '../src/lib/i18n';
import NodePage from '../src/public/NodePage.svelte';
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

describe('node page', () => {
  it('shows_the_last_round_with_votes_and_the_history_strip', async () => {
    const api = fakeApi();
    app = mount(NodePage, { target, props: { id: 'theta-m1d7z', fetchFn: api.fetchFn } });
    await settle();
    expect(target.querySelector('h1')?.textContent).toContain('theta-m1d7z');
    expect(target.querySelectorAll('.vote')).toHaveLength(2);
    // An abstention shows its reason as words, never as the raw code.
    expect(target.textContent).toContain(t('vote.reason.unavailable'));
    expect(target.textContent).not.toContain('unavailable');
    const strip = target.querySelectorAll('.hs i');
    expect(strip).toHaveLength(96);
    // A bucket with no samples is drawn as "no data", never as a failure.
    expect(target.querySelectorAll('.hs i.n').length).toBe(6);
    expect(target.querySelectorAll('.hs i.x')).toHaveLength(1);
  });

  it('unknown_node_says_so', async () => {
    const api = fakeApi();
    app = mount(NodePage, { target, props: { id: 'nope-00000', fetchFn: api.fetchFn } });
    await settle();
    expect(target.textContent).toContain(t('node.missing.title'));
    expect(target.querySelector('a[href="/"]')).not.toBeNull();
  });
});
