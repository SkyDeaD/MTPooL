// Measures the script weight of the public page from the built files and fails above 50 KB
// (gzip). The panel is not part of the budget. Also fails if the build mentions another host.
import { readdir, readFile } from 'node:fs/promises';
import { dirname, extname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';
import { findExternalUrls } from './external-urls.ts';

const BUDGET = 50 * 1024;
const dist = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');

async function* walk(dir: string): AsyncGenerator<string> {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else yield path;
  }
}

// Scripts the page loads: <script src> and <link rel="modulepreload">, then what they import.
const html = await readFile(join(dist, 'index.html'), 'utf8');
const pending = [...html.matchAll(/<(?:script[^>]*\ssrc|link[^>]*rel="modulepreload"[^>]*\shref)="([^"]+\.js)"/g)].map(
  (m) => m[1],
);
const seen = new Set<string>();
let total = 0;
while (pending.length > 0) {
  const url = pending.pop()!;
  if (seen.has(url)) continue;
  seen.add(url);
  const data = await readFile(join(dist, url));
  const size = gzipSync(data, { level: 9 }).length;
  total += size;
  console.log(`${String(size).padStart(7)} B  ${url}`);
  // Static imports of the chunk: `from"./x.js"` or `import"./x.js"`.
  for (const m of data.toString('utf8').matchAll(/(?:from|import)\s*"\.\/([^"]+\.js)"/g)) {
    pending.push(`${dirname(url)}/${m[1]}`.replace(/^\.\//, ''));
  }
}
console.log(`public page JS, gzip: ${total} B of ${BUDGET} B (${(total / 1024).toFixed(1)} KB)`);

let failed = false;
if (seen.size === 0) {
  console.error('no scripts found in dist/index.html');
  failed = true;
}
if (total > BUDGET) {
  console.error('over the budget');
  failed = true;
}

for await (const path of walk(dist)) {
  const ext = extname(path);
  if (!['.html', '.js', '.css', '.svg', '.json'].includes(ext)) continue;
  const found = findExternalUrls(await readFile(path, 'utf8'));
  if (found.length > 0) {
    console.error(`external address in ${path.slice(dist.length + 1)}: ${found.join(', ')}`);
    failed = true;
  }
}

process.exit(failed ? 1 : 0);
