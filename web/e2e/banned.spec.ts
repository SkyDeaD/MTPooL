import { expect, test } from '@playwright/test';
import { openPanel, stand } from './support.ts';

// A banned node is in the ban history and the journal, and nowhere else: not in a node table, not
// in a counter, not in "needs attention", on the public pages or in the panel.
function bannedId(): string {
  return stand.ids[stand.roster.banned];
}

test('banned: the panel node table, the rail counter and the attention list do not show it', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await openPanel(page, '#/nodes');
  const healthy = stand.ids[stand.roster.healthy[0]];
  await expect(page.getByRole('link', { name: healthy, exact: true }).first()).toBeVisible();
  await expect(page.locator('main').getByText(bannedId())).toHaveCount(0);

  // The counter in the rail counts the pool: healthy and quarantined nodes.
  const nodes: { id: string; state: string }[] = await stand.admin('/nodes');
  const inPool = nodes.filter((n) => n.state === 'healthy' || n.state === 'quarantine');
  const counter = page.locator('.rail nav a', { hasText: 'Ноды' }).locator('.cnt');
  const text = (await counter.textContent())?.trim() ?? '';
  expect(text.endsWith(String(inPool.length)), `the counter says "${text}", the pool has ${inPool.length}`).toBe(true);

  await page.goto('/panel/#/overview');
  await expect(page.locator('main h1').first()).toHaveText('Обзор');
  const attention = page.locator('section, .t').filter({ hasText: 'Требует внимания' }).first();
  await expect(attention).toBeVisible();
  await expect(attention.getByText(bannedId())).toHaveCount(0);

  await page.getByRole('search').getByRole('searchbox').fill(bannedId());
  await expect(page.locator('.srch .res')).toBeVisible();
  await expect(page.locator('.srch .res').getByText(bannedId())).toHaveCount(0);
});

test('banned: it is in the ban history and the journal of the panel', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await openPanel(page, '#/overview');
  await expect(page.locator('table.list').getByText(bannedId())).toBeVisible();

  await page.goto('/panel/#/journal');
  await expect(page.locator('main h1').first()).toHaveText('Журнал');
  await expect(page.locator('main').getByText(bannedId()).first()).toBeVisible();
});

test('banned: a row of the ban history opens the node page with the unban button', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await openPanel(page, '#/overview');
  const row = page.locator('table.list tbody tr', { hasText: bannedId() });
  await expect(row).toBeVisible();
  await row.getByText(bannedId()).click();
  await expect(page).toHaveURL(new RegExp(`#/nodes/${bannedId()}$`));
  await expect(page.locator('main h1').first()).toHaveText(bannedId());
  await expect(page.getByRole('button', { name: 'Снять бан' })).toBeVisible();
});

test('banned: the public statistics page has no trace of it', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByText(/Нод в работе/).first()).toBeVisible();
  await page.waitForLoadState('networkidle');
  await expect(page.locator('main').getByText(bannedId())).toHaveCount(0);
});

test('banned: the public dashboard lists it in the ban history only', async ({ page }) => {
  await page.goto('/dashboard/');
  await expect(page.getByText(/Дашборд банов/).first()).toBeVisible();
  await expect(page.locator('table.list').getByText(bannedId())).toBeVisible();
});
