import { expect, test } from '@playwright/test';
import { expectNoAxeViolations, expectNoSidewaysScroll, openPanel, settled, signIn, stand, THEMES, useTheme, watchExternal, WIDTHS } from './support.ts';

// The panel, served by the real registrar at /panel/. Every route renders in both themes at three
// widths; each one must fit the window, ask no other host for anything, and pass axe at WCAG AA.
const ROUTES: { hash: string; heading: RegExp }[] = [
  { hash: '#/overview', heading: /Обзор/ },
  { hash: '#/nodes', heading: /Ноды/ },
  { hash: '#/masters', heading: /Мастера и DNS/ },
  { hash: '#/srmd', heading: /СРМД/ },
  { hash: '#/journal', heading: /Журнал/ },
  { hash: '#/settings', heading: /Настройки/ },
  { hash: '#/owners', heading: /Владельцы и токены/ },
  { hash: '#/updates', heading: /Обновления/ },
];

for (const route of ROUTES) {
  for (const theme of THEMES) {
    for (const width of WIDTHS) {
      test(`panel ${route.hash}, ${theme}, ${width}px`, async ({ page }) => {
        const external = watchExternal(page);
        await useTheme(page, theme);
        await page.setViewportSize({ width, height: 900 });
        await openPanel(page, route.hash);
        await expect(page.locator('main h1').first()).toHaveText(route.heading);
        await settled(page);
        await expect(page.locator('html')).toHaveAttribute('data-theme', theme);
        await expectNoSidewaysScroll(page);
        await expectNoAxeViolations(page, `${route.hash} ${theme} ${width}`);
        expect(external, 'requests to other hosts').toEqual([]);
      });
    }
  }
}

for (const theme of THEMES) {
  for (const width of WIDTHS) {
    test(`panel node page, ${theme}, ${width}px`, async ({ page }) => {
      const external = watchExternal(page);
      await useTheme(page, theme);
      await page.setViewportSize({ width, height: 900 });
      const id = stand.ids[stand.roster.healthy[0]];
      await openPanel(page, `#/nodes/${id}`);
      await expect(page.locator('main h1').first()).toHaveText(id);
      await settled(page);
      await expectNoSidewaysScroll(page);
      await expectNoAxeViolations(page, `node ${theme} ${width}`);
      expect(external, 'requests to other hosts').toEqual([]);
    });
  }
}

test('panel login: a wrong token is refused beside the field, the right one opens the panel and survives a reload', async ({ page }) => {
  await page.goto('/panel/#/nodes');
  await page.locator('#token').fill('wrong-token-for-the-check');
  await page.locator('form button[type=submit]').click();
  await expect(page.getByRole('alert')).toContainText('Токен не подошёл');
  await expect(page.locator('.rail')).toHaveCount(0);

  await signIn(page, '#/nodes');
  await expect(page.locator('main h1').first()).toHaveText('Ноды');
  await page.reload();
  await expect(page.locator('.rail')).toBeVisible();
  await expect(page.locator('main h1').first()).toHaveText('Ноды');

  await page.getByRole('button', { name: 'Выйти' }).click();
  await expect(page.locator('#token')).toBeVisible();
  await page.reload();
  await expect(page.locator('#token')).toBeVisible();
});
