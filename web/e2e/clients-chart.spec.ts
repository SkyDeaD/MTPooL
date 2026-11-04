import { expect, test } from '@playwright/test';

// A pool that has just started has one or two samples, and the newest may still be ahead of the
// drawn line. The scale must already hold it, and its labels must be readable.
test('the scale of the clients chart holds the current number', async ({ page }) => {
  await page.goto('/');
  const chart = page.locator('.cc[role="img"]').first();
  await expect(chart.locator('.ty').first()).toBeVisible();
  await page.waitForTimeout(1500);

  const label = (await chart.getAttribute('aria-label')) ?? '';
  const current = Number((label.split(',')[0] ?? '').replace(/[^\d]/g, ''));
  const labels = (await chart.locator('.ty').allTextContents()).map((text) => text.trim());
  const ticks = labels.map((text) => Number(text.replace(/[^\d]/g, '')));

  expect(new Set(labels).size, labels.join(', ')).toBe(labels.length);
  expect(current).toBeGreaterThan(0);
  expect(Math.max(...ticks) * 1.5).toBeGreaterThanOrEqual(current);
});

// Domains rotate together, so two marks with the same time are the normal case.
test('two master changes at the same instant do not break the clients chart', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.route('**/api/v1/stats/clients*', async (route) => {
    const at = (back: number) => new Date(Date.now() - back * 1000).toISOString();
    await route.fulfill({
      json: {
        range: '10m',
        step_sec: 30,
        points: [120, 90, 60, 30].map((back) => ({ at: at(back), total: 10, domains: {} })),
        marks: [
          { at: at(100), kind: 'master_change' },
          { at: at(100), kind: 'master_change' },
        ],
      },
    });
  });
  await page.goto('/');
  await expect(page.locator('.cc[role="img"] .ty').first()).toBeVisible();
  await expect(page.locator('.cc[role="img"] .sk')).toHaveCount(0);
  expect(errors).toEqual([]);
});
