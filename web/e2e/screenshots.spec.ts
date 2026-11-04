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
