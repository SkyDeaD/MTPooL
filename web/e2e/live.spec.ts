import { expect, test } from '@playwright/test';
import { openPanel, stand } from './support.ts';

// Real events from the registrar's stream reach an open panel without a reload.
test('live: a node that registers while the panel is open appears in the table and in the journal', async ({ page }) => {
  await openPanel(page, '#/nodes');
  await expect(page.getByRole('status').filter({ hasText: 'На связи' })).toBeVisible();

  let reloaded = false;
  page.on('load', () => {
    reloaded = true;
  });

  const node = await stand.agent().enroll(`live-${Date.now().toString(36)}`, '203.0.113.60', 8443);
  try {
    await expect(page.getByRole('link', { name: node.id, exact: true }).first()).toBeVisible({ timeout: 15_000 });
    expect(reloaded, 'the page was reloaded').toBe(false);

    await page.goto('/panel/#/journal');
    await expect(page.getByText(node.id).first()).toBeVisible();
  } finally {
    await stand.admin(`/nodes/${node.id}/retire`, { method: 'POST', body: {} });
  }
});

test('live: a retired node moves to the idle table without a reload', async ({ page }) => {
  const node = await stand.agent().enroll(`live-${Date.now().toString(36)}`, '203.0.113.61', 8443);
  await page.setViewportSize({ width: 1440, height: 900 });
  await openPanel(page, '#/nodes');
  const link = page.getByRole('link', { name: node.id, exact: true }).first();
  await expect(link).toBeVisible();
  await stand.admin(`/nodes/${node.id}/retire`, { method: 'POST', body: {} });
  // Retired nodes are not in work any more: they sit under "Не в работе", not in the pool tables.
  const idle = page.locator('section.t').filter({ has: page.getByRole('heading', { name: 'Не в работе' }) });
  await expect(idle.getByRole('link', { name: node.id, exact: true })).toBeVisible({ timeout: 15_000 });
});

// The public stream of a real registrar: new numbers reach an open page without a reload or a poll.
test('live: the numbers of a node reach its public page within one tick', async ({ page }) => {
  const agent = stand.agent();
  const node = await agent.enroll(`live-${Date.now().toString(36)}`, '203.0.113.62', 8443);
  try {
    await agent.report(node, { clients: 7, other: 30 });
    let reloaded = false;
    await page.goto(`/node/?id=${node.id}`);
    page.on('load', () => {
      reloaded = true;
    });
    const kpi = page.locator('.kpi').filter({ hasText: 'Клиенты' });
    await expect(kpi.locator('.v')).toHaveText('7');
    await expect(kpi).toContainText('ещё 30 своих у владельца ноды');

    await agent.report(node, { clients: 23, other: 31 });
    // Well under the 30 seconds of a poll: one tick is 5 seconds.
    await expect(kpi.locator('.v')).toHaveText('23', { timeout: 10_000 });
    await expect(kpi).toContainText('ещё 31 свой у владельца ноды');
    expect(reloaded, 'the page was reloaded').toBe(false);
  } finally {
    await stand.admin(`/nodes/${node.id}/retire`, { method: 'POST', body: {} });
  }
});

// The service ends a stream when it is old. The browser opens a new one by itself, after the wait the
// stream told it.
test('live: a stream that ended opens again by itself after its wait', async ({ page }) => {
  let asked = 0;
  await page.route('**/api/v1/stream', (route) => {
    asked += 1;
    return route.fulfill({ status: 200, contentType: 'text/event-stream', body: 'retry: 50\n\n' });
  });
  await page.goto('/');
  await expect.poll(() => asked, { timeout: 5000 }).toBeGreaterThanOrEqual(3);
});

// A refused stream ends for good in the browser: only the page itself can ask again.
test('live: a page refused the stream keeps working and asks again later', async ({ page }) => {
  let asked = 0;
  await page.route('**/api/v1/stream', (route) => {
    asked += 1;
    return asked === 1
      ? route.fulfill({
          status: 429,
          contentType: 'application/problem+json',
          headers: { 'retry-after': '60' },
          body: '{"type":"about:blank","title":"Too many requests","status":429,"code":"rate_limited","detail":"later"}',
        })
      : route.fulfill({ status: 200, contentType: 'text/event-stream', body: 'retry: 600000\n\n' });
  });
  await page.clock.install();
  await page.goto('/');
  await expect(page.getByText(/Нод в работе/).first()).toBeVisible();
  await expect.poll(() => asked).toBe(1);
  await page.clock.runFor(61_000);
  await expect.poll(() => asked, { timeout: 5000 }).toBeGreaterThanOrEqual(2);
});
