// Writes a .br and a .gz twin next to every text file in dist. The server picks one by the
// request's Accept-Encoding. Fonts are already compressed and are left alone.
import { readdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, extname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { brotliCompressSync, constants, gzipSync } from 'node:zlib';

const dist = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');
const TEXT = new Set(['.html', '.js', '.css', '.svg', '.json', '.txt', '.map', '.xml']);

async function* walk(dir: string): AsyncGenerator<string> {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else yield path;
  }
}

let count = 0;
for await (const path of walk(dist)) {
  if (!TEXT.has(extname(path))) continue;
  const data = await readFile(path);
  const br = brotliCompressSync(data, {
    params: {
      [constants.BROTLI_PARAM_QUALITY]: 11,
      [constants.BROTLI_PARAM_SIZE_HINT]: data.length,
    },
  });
  await writeFile(`${path}.br`, br);
  await writeFile(`${path}.gz`, gzipSync(data, { level: 9 }));
  count += 1;
}
console.log(`compressed ${count} files`);
