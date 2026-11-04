// Measures the script weight of every public page from the built files and fails above 50 KB
// (gzip) for any of them. The panel is not part of the budget. Also fails if the build mentions
// another host. rapidoc-min.js is third-party code loaded only on click; it is not part of any
// page's weight and its own strings (example hosts, library urls) are not scanned.
import { readdir, readFile } from 'node:fs/promises';
import { dirname, extname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';
import { findExternalUrls } from './external-urls.ts';

const BUDGET = 50 * 1024;
const dist = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');
const PAGES = ['index.html', 'stats/index.html', 'dashboard/index.html', 'links/index.html', 'api/index.html', 'node/index.html'];
const UNSCANNED = new Set(['rapidoc-min.js']);
// Texts only the panel uses: finding one in a public page means the panel's strings leaked into it.
const PANEL_MARKERS = ['panel.overview', 'panel.nodes.title', 'settings.group.checks', 'bits-ui'];

async function* walk(dir: string): AsyncGenerator<string> {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else yield path;
  }
}

let failed = false;

// Scripts a page loads: <script src> and <link rel="modulepreload">, then what they import.
async function weigh(page: string): Promise<number> {
  const html = await readFile(join(dist, page), 'utf8');
  const pending = [...html.matchAll(/<(?:script[^>]*\ssrc|link[^>]*rel="modulepreload"[^>]*\shref)="\/?([^"]+\.js)"/g)].map(
    (m) => m[1],
  );
  const seen = new Set<string>();
  let total = 0;
  while (pending.length > 0) {
    const url = pending.pop()!;
    if (seen.has(url)) continue;
    seen.add(url);
    const data = await readFile(join(dist, url));
    total += gzipSync(data, { level: 9 }).length;
    for (const marker of PANEL_MARKERS) {
      if (data.includes(marker)) {
        console.error(`panel text "${marker}" in the script ${url} of ${page}`);
        failed = true;
      }
    }
    // Static imports of the chunk: `from"./x.js"` or `import"./x.js"`.
    for (const m of data.toString('utf8').matchAll(/(?:from|import)\s*"\.\/([^"]+\.js)"/g)) {
      pending.push(`${dirname(url)}/${m[1]}`.replace(/^\.\//, ''));
    }
  }
  if (seen.size === 0) {
    console.error(`no scripts found in dist/${page}`);
    failed = true;
  }
  return total;
}

for (const page of PAGES) {
  const total = await weigh(page);
  const over = total > BUDGET;
  if (over) failed = true;
  console.log(`${page.padEnd(22)} JS gzip: ${String(total).padStart(6)} B of ${BUDGET} B (${(total / 1024).toFixed(1)} KB)${over ? '  OVER' : ''}`);
}

for await (const path of walk(dist)) {
  if (UNSCANNED.has(path.slice(dist.length + 1))) continue;
  if (!['.html', '.js', '.css', '.svg', '.json'].includes(extname(path))) continue;
  const found = findExternalUrls(await readFile(path, 'utf8'));
  if (found.length > 0) {
    console.error(`external address in ${path.slice(dist.length + 1)}: ${found.join(', ')}`);
    failed = true;
  }
}

process.exit(failed ? 1 : 0);
