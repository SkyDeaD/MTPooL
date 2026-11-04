import { mkdirSync } from 'node:fs';
import { resolve } from 'node:path';
import { test } from '@playwright/test';
import { openPanel, stand, THEMES, useTheme } from './support.ts';

// A picture of every page at 360 and 1440 px in both themes, for a person to look at.
// Off in the normal run; `MTPOOL_SHOTS=1 npx playwright test e2e/screenshots.spec.ts` writes them
// into web/screenshots/ (ignored by git).
test.skip(!process.env.MTPOOL_SHOTS, 'set MTPOOL_SHOTS=1 to take the pictures');

const OUT = resolve(import.meta.dirname, '..', 'screenshots');
const PANEL = ['overview', 'nodes', 'masters', 'srmd', 'journal', 'settings', 'owners', 'updates'];

for (const theme of THEMES) {
  for (const width of [360, 1440]) {
    test(`pictures, ${theme}, ${width}px`, async ({ page }) => {
      mkdirSync(OUT, { recursive: true });
      await useTheme(page, theme);
      await page.setViewportSize({ width, height: 900 });
      const healthy = stand.ids[stand.roster.healthy[0]];
      const publicPages: [string, string][] = [
        ['statistics', '/'],
        ['dashboard', '/dashboard/'],
        ['links', '/links/'],
        ['api', '/api/'],
        ['node', `/node/?id=${healthy}`],
      ];
      for (const [name, path] of publicPages) {
        await page.goto(path);
        await page.waitForLoadState('networkidle');
        await page.screenshot({ path: `${OUT}/public-${name}-${theme}-${width}.png`, fullPage: true });
      }
      await openPanel(page, '#/overview');
      for (const route of PANEL) {
        await page.goto(`/panel/#/${route}`);
        await page.waitForTimeout(700);
        await page.screenshot({ path: `${OUT}/panel-${route}-${theme}-${width}.png`, fullPage: true });
      }
      await page.goto(`/panel/#/nodes/${healthy}`);
      await page.waitForTimeout(700);
      await page.screenshot({ path: `${OUT}/panel-node-${theme}-${width}.png`, fullPage: true });
    });
  }
}

// The pictures in the README, one for each theme: what a first look at the product shows.
const DOCS = resolve(import.meta.dirname, '..', '..', 'docs', 'images');
for (const theme of THEMES) {
  test(`pictures for the README, ${theme}`, async ({ page }) => {
    mkdirSync(DOCS, { recursive: true });
    await useTheme(page, theme);
    await page.setViewportSize({ width: 1280, height: 1000 });
    await page.goto('/');
    await page.waitForLoadState('networkidle');
    await page.waitForTimeout(1500);
    await page.screenshot({ path: `${DOCS}/public-${theme}.png` });
    await openPanel(page, '#/overview');
    await page.waitForTimeout(1500);
    await page.screenshot({ path: `${DOCS}/panel-${theme}.png` });

    // The gallery: one working screen each, without the side rail so it stays readable when small.
    const area = { x: 232, y: 0, width: 1048, height: 700 };
    await page.setViewportSize({ width: 1280, height: 800 });
    // The node in quarantine: it is the one with rounds of sensor votes to show.
    const checked = stand.ids[stand.roster.quarantine];
    const gallery: [string, string][] = [
      ['nodes', '#/nodes'],
      ['masters', '#/masters'],
      ['node', `#/nodes/${checked}`],
      ['journal', '#/journal'],
    ];
    for (const [name, route] of gallery) {
      await page.goto(`/panel/${route}`);
      await page.waitForTimeout(1200);
      await page.screenshot({ path: `${DOCS}/panel-${name}-${theme}.png`, clip: area });
    }
    await page.goto('/panel/#/settings');
    await page.getByRole('tab', { name: 'Домены и DNS' }).click();
    await page.waitForTimeout(800);
    await page.screenshot({ path: `${DOCS}/panel-settings-${theme}.png`, clip: area });
    await page.setViewportSize({ width: 1048, height: 700 });
    await page.goto('/links/');
    await page.waitForLoadState('networkidle');
    await page.waitForTimeout(800);
    await page.screenshot({ path: `${DOCS}/public-links-${theme}.png` });
  });
}
