import { expect, test, type Page } from '@playwright/test';
import { openPanel } from './support.ts';

// The panel with the keyboard alone: every stop shows where the focus is, "/" opens the search,
// and a dialog closes on Escape and gives the focus back.
async function focusInfo(page: Page) {
  return page.evaluate(() => {
    const el = document.activeElement as HTMLElement | null;
    if (!el || el === document.body) return null;
    const style = getComputedStyle(el);
    return {
      text: (el.textContent || el.getAttribute('aria-label') || '').trim().slice(0, 40),
      outlined: style.outlineStyle !== 'none' && parseFloat(style.outlineWidth) > 0,
      shadow: style.boxShadow !== 'none',
    };
  });
}

test('keyboard: Tab walks through the navigation and every stop shows the focus', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await openPanel(page, '#/overview');
  const seen: string[] = [];
  for (let i = 0; i < 14; i++) {
    await page.keyboard.press('Tab');
    const info = await focusInfo(page);
    expect(info, 'the focus fell off the page').not.toBeNull();
    expect(info!.outlined || info!.shadow, `no visible focus on "${info!.text}"`).toBe(true);
    seen.push(info!.text);
  }
  for (const label of ['Обзор', 'Ноды', 'Мастера и DNS', 'СРМД', 'Журнал', 'Настройки', 'Владельцы и токены', 'Обновления']) {
    expect(seen.some((s) => s.includes(label)), `Tab never reached "${label}": ${seen.join(' | ')}`).toBe(true);
  }
});

test('keyboard: Enter on a navigation link changes the page', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await openPanel(page, '#/overview');
  await page.locator('.rail nav a', { hasText: 'Журнал' }).focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('main h1').first()).toHaveText('Журнал');
});

test('keyboard: "/" moves the focus to the search', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await openPanel(page, '#/overview');
  await page.locator('main h1').first().click();
  await page.keyboard.press('/');
  await expect(page.getByRole('searchbox', { name: 'Поиск по панели' })).toBeFocused();
});

test('keyboard: a dialog opens from the keyboard, closes on Escape and returns the focus', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await openPanel(page, '#/nodes');
  const opener = page.getByRole('button', { name: 'Подключить ноду' }).first();
  await opener.focus();
  await page.keyboard.press('Enter');
  const dialog = page.getByRole('dialog', { name: 'Подключить ноду' });
  await expect(dialog).toBeVisible();
  // The focus is inside the dialog.
  expect(await dialog.evaluate((el) => el.contains(document.activeElement))).toBe(true);
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  await expect(opener).toBeFocused();
});
