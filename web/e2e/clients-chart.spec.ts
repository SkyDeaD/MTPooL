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
