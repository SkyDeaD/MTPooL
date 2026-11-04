import { expect, test } from '@playwright/test';
import { expectNoAxeViolations, expectNoSidewaysScroll, settled, stand, THEMES, useTheme, watchExternal, WIDTHS } from './support.ts';

// The public pages, from a real registrar: every page in both themes at three widths. Each one
// must fit the window, ask no other host for anything, and pass axe at WCAG AA.
const PAGES: { name: string; path: () => string; heading: RegExp }[] = [
  { name: 'statistics', path: () => '/', heading: /Нод в работе/ },
  { name: 'dashboard', path: () => '/dashboard/', heading: /Дашборд банов/ },
  { name: 'links', path: () => '/links/', heading: /Ссылки для подключения/ },
  { name: 'api', path: () => '/api/', heading: /Быстрый старт/ },
  { name: 'node', path: () => `/node/?id=${stand.ids[stand.roster.healthy[0]]}`, heading: /Доступность/ },
];

for (const page of PAGES) {
  for (const theme of THEMES) {
    for (const width of WIDTHS) {
      test(`public ${page.name}, ${theme}, ${width}px`, async ({ page: tab }) => {
        const external = watchExternal(tab);
        await useTheme(tab, theme);
        await tab.setViewportSize({ width, height: 900 });
        await tab.goto(page.path());
        await expect(tab.getByText(page.heading).first()).toBeVisible();
        await settled(tab);
        await expect(tab.locator('html')).toHaveAttribute('data-theme', theme);
        await expectNoSidewaysScroll(tab);
        await expectNoAxeViolations(tab, `${page.name} ${theme} ${width}`);
        expect(external, 'requests to other hosts').toEqual([]);
      });
    }
  }
}

test('public: the statistics page shows the pool of the stand and no banned node', async ({ page }) => {
  await page.goto('/');
  const { healthy, quarantine, banned } = stand.roster;
  const main = page.locator('main');
  await expect(main.getByRole('link', { name: stand.ids[healthy[0]], exact: true })).toBeVisible();
  await expect(main.getByRole('link', { name: stand.ids[quarantine], exact: true })).toBeVisible();
  await expect(main.getByText(stand.ids[banned])).toHaveCount(0);
  // The counter agrees: the pool holds the healthy ones and the one in quarantine, not the banned.
  await expect(main.getByText(`из ${healthy.length + 1}`)).toBeVisible();
});

test('public: keyboard reaches every tab of the header and shows where the focus is', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByText(/Нод в работе/).first()).toBeVisible();
  const seen: string[] = [];
  for (let i = 0; i < 8; i++) {
    await page.keyboard.press('Tab');
    const info = await page.evaluate(() => {
      const el = document.activeElement as HTMLElement | null;
      if (!el || el === document.body) return null;
      const style = getComputedStyle(el);
      const outlined = style.outlineStyle !== 'none' && parseFloat(style.outlineWidth) > 0;
      return { text: (el.textContent ?? '').trim().slice(0, 30), outlined, shadow: style.boxShadow !== 'none' };
    });
    expect(info, 'focus fell off the page').not.toBeNull();
    expect(info!.outlined || info!.shadow, `no visible focus on "${info!.text}"`).toBe(true);
    seen.push(info!.text);
  }
  for (const label of ['Статистика', 'Дашборд', 'Ссылки', 'API', 'Панель']) {
    expect(seen.some((s) => s.includes(label)), `Tab never reached "${label}": ${seen.join(' | ')}`).toBe(true);
  }
});
