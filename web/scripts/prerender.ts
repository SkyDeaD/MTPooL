// Fills dist/index.html with the markup of the public page, so it is readable before any
// script runs. The client bundle then hydrates it.
import { readFile, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const htmlPath = join(root, 'dist', 'index.html');
const entry = join(root, '.ssr', 'entry-server.js');

const { render } = (await import(pathToFileURL(entry).href)) as {
  render: () => { head: string; body: string };
};
const { head, body } = render();

const template = await readFile(htmlPath, 'utf8');
if (!template.includes('<!--app-->') || !template.includes('<!--head-->')) {
  throw new Error('dist/index.html has no <!--app--> or <!--head--> placeholder');
}
// Replacer functions: the markup may contain `$` sequences.
const page = template.replace('<!--head-->', () => head).replace('<!--app-->', () => body);
await writeFile(htmlPath, page);
console.log(`prerendered dist/index.html (${page.length} bytes)`);
