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
