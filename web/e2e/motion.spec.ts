import { expect, test } from '@playwright/test';
import { openPanel } from './support.ts';

// With `prefers-reduced-motion: reduce` the running dots of the scheme stand still.
async function runningAnimations(page: import('@playwright/test').Page): Promise<{ total: number; infinite: number }> {
  return page.evaluate(() => {
    const all = document.getAnimations();
    const infinite = all.filter((a) => {
      const timing = a.effect?.getComputedTiming();
      return timing !== undefined && timing.iterations === Infinity && a.playState === 'running';
    }).length;
    return { total: all.length, infinite };
  });
}

test('motion: the dots of the scheme run by default', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await openPanel(page, '#/overview');
  await expect(page.locator('.run').first()).toBeAttached();
  await expect.poll(async () => (await runningAnimations(page)).infinite).toBeGreaterThan(0);
});

test('motion: with reduced motion nothing runs forever', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await openPanel(page, '#/overview');
  await expect(page.locator('.run').first()).toBeAttached();
  await page.waitForTimeout(500);
  expect((await runningAnimations(page)).infinite).toBe(0);
});

test('motion: the public page also stands still with reduced motion', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.goto('/');
  await expect(page.getByText(/Нод в работе/).first()).toBeVisible();
  await page.waitForTimeout(500);
  expect((await runningAnimations(page)).infinite).toBe(0);
});
