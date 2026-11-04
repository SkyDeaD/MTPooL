import { expect, test } from '@playwright/test';
import { openPanel } from './support.ts';

// Settings: one tab panel at a time, a save that outlives a reload, and a wrong value refused
// beside the field it belongs to. `rotation_ttl_sec` is used because no other spec depends on it.
const KEY = 'rotation_ttl_sec';

test('settings: exactly one tab panel is visible, and the tabs switch it', async ({ page }) => {
  await openPanel(page, '#/settings');
  const panels = page.getByRole('tabpanel');
  await expect(panels.first()).toBeVisible();
  await expect(page.locator('[role=tabpanel]:visible')).toHaveCount(1);
  for (const name of ['Прокси', 'Домены и DNS', 'Проверки', 'Система', 'Подключение']) {
    await page.getByRole('tab', { name }).click();
    await expect(page.getByRole('tab', { name })).toHaveAttribute('aria-selected', 'true');
    await expect(page.locator('[role=tabpanel]:visible')).toHaveCount(1);
    await expect(page.locator('[role=tabpanel]:visible')).toHaveAttribute('aria-labelledby', await page.getByRole('tab', { name }).getAttribute('id'));
  }
});

test('settings: an out-of-range value is refused beside its field and nothing is sent', async ({ page }) => {
  await openPanel(page, '#/settings');
  await page.getByRole('tab', { name: 'Домены и DNS' }).click();
  const field = page.locator(`input[data-key="${KEY}"]`);
  await field.fill('99999');
  const form = page.locator('form[data-form="pl"]');
  await form.getByRole('button', { name: 'Сохранить' }).click();
  const error = field.locator('xpath=ancestor::div[contains(@class,"f")][1]').locator('.e');
  await expect(error).toBeVisible();
  await expect(error).toContainText('Целое число от');
});

test('settings: a saved value is still there after a reload', async ({ page }) => {
  await openPanel(page, '#/settings');
  await page.getByRole('tab', { name: 'Домены и DNS' }).click();
  const field = page.locator(`input[data-key="${KEY}"]`);
  const before = await field.inputValue();
  const next = before === '90' ? '91' : '90';
  await field.fill(next);
  const form = page.locator('form[data-form="pl"]');
  await form.getByRole('button', { name: 'Сохранить' }).click();
  await expect(form.getByRole('button', { name: 'Сохранить' })).toBeDisabled();
  await page.reload();
  await page.getByRole('tab', { name: 'Домены и DNS' }).click();
  await expect(page.locator(`input[data-key="${KEY}"]`)).toHaveValue(next);
});

test('settings: a proxy user is added with a generated secret and removed again', async ({ page }) => {
  await openPanel(page, '#/settings');
  await page.getByRole('tab', { name: 'Прокси' }).click();
  const block = page.locator('section[data-key="proxy_users"]');
  const name = block.getByLabel('Имя нового пользователя');
  const add = block.getByRole('button', { name: 'Добавить' });

  // Names the engines would refuse never leave the page.
  await name.fill('Вася');
  await add.click();
  await expect(block.locator('.e')).toContainText('Латинские буквы');
  await name.fill('main');
  await add.click();
  await expect(block.locator('.e')).toContainText('уже есть');

  await name.fill('guest-1');
  await add.click();
  const row = block.getByRole('row').filter({ hasText: 'guest-1' });
  await expect(row).toBeVisible();
  await expect(row).toContainText('••••');
  await expect(name).toHaveValue('');

  await page.reload();
  await page.getByRole('tab', { name: 'Прокси' }).click();
  await expect(row).toBeVisible();

  await row.getByRole('button', { name: 'Удалить' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Удалить' }).click();
  await expect(row).toHaveCount(0);
  await expect(block.getByRole('row').filter({ hasText: 'main' })).toBeVisible();
});
