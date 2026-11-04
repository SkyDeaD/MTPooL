import { expect, test } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import { openPanel } from './support.ts';

const OPENAPI = fileURLToPath(new URL('../../docs/openapi.json', import.meta.url));

// Going from tab to tab must not look like a page being built again: nothing flies in, and what
// was already seen in this session is there at once while the fresh answer is on its way.
const PUBLIC = ['/', '/dashboard', '/links', '/api'];

test('public pages: a tile does not fly in when the page opens', async ({ page }) => {
  for (const path of PUBLIC) {
    await page.goto(path);
    await expect(page.locator('main .t').first()).toBeVisible();
    const flying = await page.evaluate(() =>
      [...document.querySelectorAll('main .view, main .t')].flatMap((e) => e.getAnimations()).map((a) => (a as CSSAnimation).animationName),
    );
    expect(flying, path).toEqual([]);
  }
});

test('public pages: coming back shows the last answer before the new one arrives', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('section.hero')).toBeVisible();
  await expect(page.locator('main .sk')).toHaveCount(0);
  await page.getByRole('navigation').getByRole('link', { name: 'Дашборд' }).click();
  await expect(page).toHaveURL(/dashboard/);

  // From here on the nodes never answer: only what the session remembers can draw the tile.
  let release = () => {};
  const held = new Promise<void>((resolve) => (release = resolve));
  await page.route('**/api/v1/nodes', async (route) => {
    await held;
    await route.abort();
  });
  await page.getByRole('navigation').getByRole('link', { name: 'Статистика' }).click();
  await expect(page.locator('section.hero')).toBeVisible();
  await expect(page.locator('html')).not.toHaveClass(/back/);
  release();
});

test('API page: the button shows a working form for the request', async ({ page }) => {
  // The specs before this one use up the stand's limit on public requests, and the limit is not
  // what is tested here: the document and the answer come from the test.
  await page.route('**/api/v1/openapi.json', (route) => route.fulfill({ path: OPENAPI, contentType: 'application/json' }));
  await page.route('**/api/v1/nodes', (route) => route.fulfill({ json: [] }));
  await page.goto('/api');
  await page.locator('button[data-tryit]').first().click();
  const doc = page.locator('rapi-doc');
  await expect(doc).toBeVisible();
  await expect.poll(() => page.evaluate(() => customElements.get('rapi-doc') !== undefined)).toBe(true);
  expect((await doc.boundingBox())!.height).toBeGreaterThan(200);
  // The real thing: the request is sent and the answer is shown.
  const answer = page.waitForResponse((r) => r.url().endsWith('/api/v1/nodes') && r.request().resourceType() === 'fetch');
  await doc.getByRole('button', { name: /try/i }).first().click();
  expect((await answer).status()).toBe(200);
  await expect(doc.getByText(/RESPONSE|200/i).first()).toBeVisible();
});

test('every page has an icon for the browser tab', async ({ page, request }) => {
  for (const path of [...PUBLIC, '/panel/']) {
    await page.goto(path);
    const href = await page.locator('link[rel="icon"]').getAttribute('href');
    expect(href, path).toBeTruthy();
    const answer = await request.get(href!);
    expect(answer.status(), path).toBe(200);
    expect(answer.headers()['content-type'], path).toContain('image/svg+xml');
  }
});

test('panel: the words of a journal tab sit in the middle of it', async ({ page }) => {
  await openPanel(page, '#/journal');
  const tab = page.getByRole('tablist').getByRole('tab').first();
  await expect(tab).toBeVisible();
  const off = await tab.evaluate((el) => {
    const range = document.createRange();
    range.selectNodeContents(el);
    const words = range.getBoundingClientRect();
    const box = el.getBoundingClientRect();
    return Math.abs(words.top + words.height / 2 - (box.top + box.height / 2));
  });
  expect(off).toBeLessThan(1.5);
});

test('panel: nothing from the admin API is kept in session storage', async ({ page }) => {
  await openPanel(page, '#/settings');
  await expect(page.getByRole('tab', { name: 'Прокси' })).toBeVisible();
  const keys = await page.evaluate(() => Object.keys(sessionStorage));
  expect(keys.filter((k) => k.includes('admin'))).toEqual([]);
});

test('public pages: the header lines up with the page under it', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto('/dashboard');
  const left = async (selector: string) => (await page.locator(selector).first().boundingBox())!.x;
  await expect(page.locator('main .t').first()).toBeVisible();
  expect(Math.abs((await left('.phd .logo')) - (await left('main .view')))).toBeLessThan(1);
});

test('public pages: the thumb of a switch starts under the chosen button without sliding', async ({ page }) => {
  await page.goto('/dashboard');
  const moving = await page.evaluate(() =>
    new Promise<number>((resolve) => requestAnimationFrame(() => {
      resolve([...document.querySelectorAll('.seg .thumb')].flatMap((e) => e.getAnimations()).length);
    })),
  );
  expect(moving).toBe(0);
  const seg = page.locator('.seg').first();
  const thumb = (await seg.locator('.thumb').boundingBox())!;
  const chosen = (await seg.locator('button[aria-pressed="true"]').boundingBox())!;
  expect(Math.abs(thumb.x - chosen.x)).toBeLessThan(1.5);
  expect(Math.abs(thumb.width - chosen.width)).toBeLessThan(1.5);
});
