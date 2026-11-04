// @vitest-environment jsdom
import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { t } from '../src/lib/i18n';
import Links from '../src/public/Links.svelte';
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

describe('links page', () => {
  it('links_copy_and_qr_work_without_app', async () => {
    const writeText = vi.fn(async () => {});
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
    const api = fakeApi();
    app = mount(Links, { target, props: { fetchFn: api.fetchFn } });
    await settle();

    // Opening in Telegram is a plain link, so it works with no script.
    const open = [...target.querySelectorAll<HTMLAnchorElement>('a[href^="tg://"]')];
    expect(open).toHaveLength(3);
    expect(open[0].getAttribute('href')).toContain('proxy1.example.com');

    // The QR code is an image from the server.
    const qr = target.querySelector<HTMLImageElement>('img.qr-img');
    expect(qr?.getAttribute('src')).toBe('/api/v1/links/l1/qr.svg');
    expect(qr?.getAttribute('alt')).toContain('proxy1.example.com');

    // Copying goes to the clipboard and tells about it.
    const copy = [...target.querySelectorAll<HTMLButtonElement>('button')].find(
      (b) => b.textContent?.trim() === t('links.copy'),
    );
    copy!.click();
    await settle();
    expect(writeText).toHaveBeenCalledWith(expect.stringContaining('proxy1.example.com'));
    expect(target.querySelector('[role="status"]')?.textContent).toContain('proxy1');
  });

  it('shows_an_explanation_when_there_are_no_links', async () => {
    const api = fakeApi();
    api.set('links', []);
    app = mount(Links, { target, props: { fetchFn: api.fetchFn } });
    await settle();
    expect(target.querySelector('.empty')?.textContent).toContain(t('links.empty.title'));
  });
});
