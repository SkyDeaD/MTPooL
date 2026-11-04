import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { findExternalUrls } from '../scripts/external-urls';

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sources(path);
    return /\.(ts|svelte|css|html)$/.test(name) && !name.endsWith('api-types.ts') ? [path] : [];
  });
}

describe('no_hardcoded_external_urls', () => {
  it('finds external addresses and ignores the allowed ones', () => {
    const text = [
      'a https://cdn.example.org/lib.js b',
      'url(//fonts.example.org/x.css)',
      '@import "http://x.example.net/y.css"',
      'xmlns="http://www.w3.org/2000/svg"',
      'see https://github.com/SkyDeaD/MTPooL for the code',
      'a local /api/v1/nodes link',
    ].join('\n');
    expect(findExternalUrls(text)).toEqual([
      'https://cdn.example.org/lib.js',
      '//fonts.example.org/x.css',
      'http://x.example.net/y.css',
    ]);
  });

  it('does not take a comment slash or a path for a protocol-relative url', () => {
    expect(findExternalUrls('a // comment\nb = "x//y"\nhref="/a//b"')).toEqual([]);
  });

  it('finds none in the sources of the interface', () => {
    const found = ['src', 'index.html', 'panel'].flatMap((entry) => {
      const stat = statSync(entry);
      const files = stat.isDirectory() ? sources(entry) : [entry];
      return files.flatMap((file) =>
        findExternalUrls(readFileSync(file, 'utf8')).map((url) => `${file}: ${url}`),
      );
    });
    expect(found).toEqual([]);
  });
});
