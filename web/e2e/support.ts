// What every spec needs: the stand's addresses, a way into the panel, and the three universal
// checks (no sideways scroll, no request to another host, no axe violation at AA).
import AxeBuilder from '@axe-core/playwright';
import { expect, type Page } from '@playwright/test';
import { Agent, type SimNode } from './stand/agent.ts';

export const THEMES = ['light', 'dark'] as const;
export const WIDTHS = [360, 768, 1440] as const;
export type Theme = (typeof THEMES)[number];

export interface Roster {
  healthy: string[];
  queued: string;
  quarantine: string;
  banned: string;
}

function need(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set: the stand did not start`);
  return value;
}

export const stand = {
  get base(): string {
    return need('MTPOOL_STAND_BASE');
  },
  get fakes(): string {
    return need('MTPOOL_STAND_FAKES_BASE');
  },
  get token(): string {
    return need('MTPOOL_STAND_ADMIN_TOKEN');
  },
  get enrollToken(): string {
    return need('MTPOOL_STAND_ENROLL_TOKEN');
  },
  get ids(): Record<string, string> {
    return JSON.parse(need('MTPOOL_STAND_IDS'));
  },
  get roster(): Roster {
    return JSON.parse(need('MTPOOL_STAND_ROSTER'));
  },
  agent(): Agent {
    return new Agent(this.base, this.fakes, this.enrollToken);
  },
  async admin(path: string, init: { method?: string; body?: unknown } = {}): Promise<any> {
    const res = await fetch(`${this.base}/api/v1/admin${path}`, {
      method: init.method ?? (init.body === undefined ? 'GET' : 'POST'),
      headers: { authorization: `Bearer ${this.token}`, 'content-type': 'application/json' },
      body: init.body === undefined ? undefined : JSON.stringify(init.body),
    });
    const text = await res.text();
    if (!res.ok) throw new Error(`admin ${path}: ${res.status} ${text}`);
    return text ? JSON.parse(text) : null;
  },
};

export type { SimNode };

/** Sets the theme the way the page's own toggle does, before any script of the page runs. */
export async function useTheme(page: Page, theme: Theme): Promise<void> {
  await page.addInitScript((value) => {
    try {
      localStorage.setItem('mtpool-theme', value);
    } catch {
      // storage blocked: the emulated colour scheme below still decides
    }
  }, theme);
  await page.emulateMedia({ colorScheme: theme });
}

/** Types the admin token into the sign-in form. The token is read from the environment, never shown. */
export async function signIn(page: Page, hash = '#/'): Promise<void> {
  await page.goto(`/panel/${hash}`);
  await page.locator('#token').fill(stand.token);
  await page.locator('form button[type=submit]').click();
  await expect(page.locator('.rail')).toBeVisible();
}

/** Opens a session without the form (the form itself is covered in panel.spec.ts) and goes to `hash`. */
export async function openPanel(page: Page, hash = '#/'): Promise<void> {
  const res = await page.request.post('/api/v1/admin/session', { data: { token: stand.token } });
  expect(res.ok(), 'the session was not opened').toBe(true);
  await page.goto(`/panel/${hash}`);
  await expect(page.locator('.rail:visible, .bnav:visible').first()).toBeVisible();
}

/** Every request to a host other than the stand's own, collected while the page works. */
export function watchExternal(page: Page): string[] {
  const external: string[] = [];
  const own = new URL(stand.base).host;
  page.on('request', (request) => {
    const url = new URL(request.url());
    if (['http:', 'https:', 'ws:', 'wss:'].includes(url.protocol) && url.host !== own) external.push(request.url());
  });
  return external;
}

export async function expectNoSidewaysScroll(page: Page): Promise<void> {
  const over = await page.evaluate(() => {
    const doc = document.documentElement;
    return { scroll: doc.scrollWidth, client: doc.clientWidth, body: document.body.scrollWidth };
  });
  expect(over.scroll, `the page is wider (${over.scroll}) than the window (${over.client})`).toBeLessThanOrEqual(over.client);
  expect(over.body).toBeLessThanOrEqual(over.client);
}

export async function expectNoAxeViolations(page: Page, label: string): Promise<void> {
  const result = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa']).analyze();
  const lines = result.violations.map(
    (v) => `${v.id} (${v.impact}): ${v.help}\n${v.nodes.slice(0, 4).map((n) => `    ${n.target.join(' ')}`).join('\n')}`,
  );
  expect(lines, `axe, ${label}`).toEqual([]);
}

/** Waits until the page has stopped loading what it shows. */
export async function settled(page: Page): Promise<void> {
  await page.waitForLoadState('networkidle');
}
