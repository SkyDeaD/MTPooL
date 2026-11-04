// Fills the built public pages with their markup, so each is readable before any script runs.
// The client bundle then hydrates it. /stats is the same page as /, so it gets a copy: the server
// has no fallback for public routes and looks for <route>/index.html.
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const dist = join(root, 'dist');
const entry = join(root, '.ssr', 'entry-server.js');

type PageName = 'stats' | 'dashboard' | 'links' | 'api' | 'node';
const { render } = (await import(pathToFileURL(entry).href)) as {
  render: (page: PageName) => { head: string; body: string };
};

const pages: { page: PageName; file: string }[] = [
  { page: 'stats', file: 'index.html' },
  { page: 'dashboard', file: 'dashboard/index.html' },
  { page: 'links', file: 'links/index.html' },
  { page: 'api', file: 'api/index.html' },
  { page: 'node', file: 'node/index.html' },
];

for (const { page, file } of pages) {
  const path = join(dist, file);
  const { head, body } = render(page);
  const template = await readFile(path, 'utf8');
  if (!template.includes('<!--app-->') || !template.includes('<!--head-->')) {
    throw new Error(`dist/${file} has no <!--app--> or <!--head--> placeholder`);
  }
  // Replacer functions: the markup may contain `$` sequences.
  const html = template.replace('<!--head-->', () => head).replace('<!--app-->', () => body);
  await writeFile(path, html);
  console.log(`prerendered dist/${file} (${html.length} bytes)`);
  if (page === 'stats') {
    await mkdir(join(dist, 'stats'), { recursive: true });
    await writeFile(join(dist, 'stats', 'index.html'), html);
    console.log('prerendered dist/stats/index.html (copy)');
  }
}
