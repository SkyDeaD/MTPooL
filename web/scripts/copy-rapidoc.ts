// Puts RapiDoc (MIT, a dev dependency) into the build as /rapidoc-min.js. The API page loads it
// only when someone asks to try a request; nothing is fetched from a CDN.
import { copyFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
await copyFile(
  join(root, 'node_modules', 'rapidoc', 'dist', 'rapidoc-min.js'),
  join(root, 'dist', 'rapidoc-min.js'),
);
console.log('copied rapidoc-min.js into dist');
