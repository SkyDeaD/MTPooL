import { defineConfig, devices } from '@playwright/test';
import { ports } from './e2e/stand/stand.ts';

// The checks run against a real registrar that serves the built `web/dist` (see e2e/stand). One
// worker: the specs share one pool, and some of them change it.
const base = `http://127.0.0.1:${ports().registrar}`;

export default defineConfig({
  testDir: 'e2e',
  testMatch: '**/*.spec.ts',
  globalSetup: './e2e/global-setup.ts',
  workers: 1,
  fullyParallel: false,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [['list']],
  outputDir: 'test-results',
  use: {
    baseURL: base,
    locale: 'ru-RU',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
