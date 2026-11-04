// Speed of the pages as a phone sees them: a real registrar serving the built `web/dist`, a phone
// profile, the processor slowed 4 times and a slow mobile network. Reports the largest contentful
// paint (LCP) and the interaction to next paint (INP) of every page and exits with 1 when one of
// them is over its limit (LCP 2.5 s, INP 200 ms).
//
//   npm run build && npm run perf
//
// The stand (see e2e/stand) is the same one the browser checks use; the admin token is made at
// the start and is never printed.
import { chromium, devices, type Page } from '@playwright/test';
import { startStand } from '../e2e/stand/stand.ts';

const LCP_LIMIT_MS = 2500;
const INP_LIMIT_MS = 200;
const RUNS = 3;

// The profile, written down so the report can say what was measured.
const PROFILE = {
  device: 'Pixel 5',
  cpuSlowdown: 4,
  // Slow 4G, the figures of the usual "slow mobile" preset: 1.6 Mbit/s down, 0.75 Mbit/s up, 150 ms round trip.
  downloadBytesPerSec: (1.6 * 1024 * 1024) / 8,
  uploadBytesPerSec: (0.75 * 1024 * 1024) / 8,
  latencyMs: 150,
};

const OBSERVE = `
  window.__lcp = 0;
  window.__events = [];
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) window.__lcp = e.startTime;
  }).observe({ type: 'largest-contentful-paint', buffered: true });
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) if (e.interactionId > 0) window.__events.push({ id: e.interactionId, d: e.duration });
  }).observe({ type: 'event', durationThreshold: 16, buffered: true });
`;

interface Target {
  name: string;
  path: string;
  panel?: boolean;
}

function inp(events: { id: number; d: number }[]): number | null {
  const worst = new Map<number, number>();
  for (const e of events) worst.set(e.id, Math.max(worst.get(e.id) ?? 0, e.d));
  const sorted = [...worst.values()].sort((a, b) => b - a);
  if (sorted.length === 0) return null;
  return sorted[Math.min(Math.floor(sorted.length / 50), sorted.length - 1)];
}

async function interact(page: Page): Promise<void> {
  // Buttons that stay on the page (theme, ranges, tabs, search), then the keyboard.
  const buttons = page.locator('main button:visible, header button:visible, .rail button:visible, .bnav button:visible');
  const count = Math.min(await buttons.count(), 4);
  for (let i = 0; i < count; i++) {
    const before = page.url();
    await buttons.nth(i).click({ timeout: 3000 }).catch(() => undefined);
    await page.waitForTimeout(400);
    if (page.url() !== before) await page.goBack().catch(() => undefined);
  }
  for (let i = 0; i < 3; i++) {
    await page.keyboard.press('Tab');
    await page.waitForTimeout(150);
  }
  await page.waitForTimeout(300);
}

async function measure(base: string, token: string, target: Target): Promise<{ lcp: number; inp: number | null }> {
  const browser = await chromium.launch();
  try {
    const context = await browser.newContext({ ...devices[PROFILE.device], locale: 'ru-RU', baseURL: base });
    if (target.panel) {
      const res = await context.request.post('/api/v1/admin/session', { data: { token } });
      if (!res.ok()) throw new Error('the session was not opened');
    }
    const page = await context.newPage();
    await page.addInitScript(OBSERVE);
    const cdp = await context.newCDPSession(page);
    await cdp.send('Emulation.setCPUThrottlingRate', { rate: PROFILE.cpuSlowdown });
    await cdp.send('Network.enable');
    await cdp.send('Network.emulateNetworkConditions', {
      offline: false,
      latency: PROFILE.latencyMs,
      downloadThroughput: PROFILE.downloadBytesPerSec,
      uploadThroughput: PROFILE.uploadBytesPerSec,
    });
    await page.goto(target.path, { waitUntil: 'load' });
    // Wait for the page to be drawn with its data: the panel keeps a stream open, so no idle wait.
    await page.waitForTimeout(target.panel ? 4000 : 2500);
    const lcp: number = await page.evaluate('window.__lcp');
    await interact(page);
    const events = (await page.evaluate('window.__events')) as { id: number; d: number }[];
    return { lcp, inp: inp(events) };
  } finally {
    await browser.close();
  }
}

const median = (xs: number[]) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

async function main(): Promise<void> {
  const stand = await startStand();
  let failed = false;
  try {
    const healthy = stand.ids[stand.roster.healthy[0]];
    const targets: Target[] = [
      { name: 'statistics', path: '/' },
      { name: 'dashboard', path: '/dashboard/' },
      { name: 'links', path: '/links/' },
      { name: 'api', path: '/api/' },
      { name: 'node', path: `/node/?id=${healthy}` },
      { name: 'panel overview', path: '/panel/#/overview', panel: true },
      { name: 'panel nodes', path: '/panel/#/nodes', panel: true },
    ];
    console.log(
      `profile: ${PROFILE.device}, processor x${PROFILE.cpuSlowdown} slower, network ${(PROFILE.downloadBytesPerSec * 8) / 1024 / 1024} Mbit/s down, ${PROFILE.latencyMs} ms round trip, median of ${RUNS} runs`,
    );
    console.log(`limits: LCP < ${LCP_LIMIT_MS} ms, INP < ${INP_LIMIT_MS} ms`);
    for (const target of targets) {
      const runs: { lcp: number; inp: number | null }[] = [];
      for (let i = 0; i < RUNS; i++) runs.push(await measure(stand.base, stand.adminToken, target));
      const lcp = median(runs.map((r) => r.lcp));
      const inps = runs.map((r) => r.inp).filter((v): v is number => v !== null);
      const worstInp = inps.length ? median(inps) : null;
      const lcpOk = lcp < LCP_LIMIT_MS;
      const inpOk = worstInp === null || worstInp < INP_LIMIT_MS;
      if (!lcpOk || !inpOk) failed = true;
      console.log(
        `${lcpOk && inpOk ? 'ok  ' : 'OVER'} ${target.name.padEnd(15)} LCP ${lcp.toFixed(0).padStart(5)} ms (runs ${runs.map((r) => r.lcp.toFixed(0)).join(', ')})  INP ${worstInp === null ? 'no interaction measured' : `${worstInp.toFixed(0)} ms (runs ${inps.map((v) => v.toFixed(0)).join(', ')})`}`,
      );
    }
  } finally {
    await stand.stop();
  }
  if (failed) {
    console.log('perf: a limit is exceeded');
    process.exitCode = 1;
  }
}

await main();
