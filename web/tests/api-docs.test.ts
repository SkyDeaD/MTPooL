// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { messages } from '../src/strings.ru';
import { buildApiRoutes } from '../scripts/gen-api-docs';
import ApiDocs from '../src/public/ApiDocs.svelte';

const openapi = JSON.parse(readFileSync(join(process.cwd(), '..', 'docs', 'openapi.json'), 'utf8'));

let app: ReturnType<typeof mount> | undefined;
let target: HTMLElement;

beforeEach(() => {
  target = document.createElement('div');
  document.body.append(target);
});
afterEach(() => {
  if (app) unmount(app);
  app = undefined;
  target.remove();
  document.querySelectorAll('script').forEach((s) => s.remove());
});

/** Operations without `security`, the ones anyone can call. */
function publicOperations(): string[] {
  const out: string[] = [];
  for (const [path, item] of Object.entries<Record<string, { security?: unknown }>>(openapi.paths)) {
    for (const [method, op] of Object.entries(item)) {
      if (!op.security) out.push(`${method.toUpperCase()} ${path}`);
    }
  }
  return out;
}

describe('api docs', () => {
  it('api_docs_page_lists_all_public_paths', () => {
    app = mount(ApiDocs, { target });
    const text = target.textContent ?? '';
    const ops = publicOperations();
    expect(ops.length).toBeGreaterThanOrEqual(10);
    for (const op of ops) expect(text, op).toContain(op);
    // Nothing of the admin or agent routes is documented for the public.
    expect(text).not.toContain('/api/v1/admin');
    expect(text).not.toContain('/api/v1/agent');
  });

  it('every_documented_route_has_a_russian_description', () => {
    const { pub, bot } = buildApiRoutes(openapi);
    for (const r of [...pub, ...bot]) {
      const key = `api.route.${r.method} ${r.path}`;
      expect(key in messages, key).toBe(true);
    }
  });

  it('tryit_script_not_loaded_until_click', async () => {
    app = mount(ApiDocs, { target });
    const tryit = () => document.querySelector('script[src="/rapidoc-min.js"]');
    expect(tryit()).toBeNull();
    const button = target.querySelector<HTMLButtonElement>('button[data-tryit]');
    expect(button).not.toBeNull();
    button!.click();
    await vi.waitFor(() => expect(tryit()).not.toBeNull());
    expect(document.querySelectorAll('script[src="/rapidoc-min.js"]')).toHaveLength(1);
    // A second click does not add the script again.
    button!.click();
    expect(document.querySelectorAll('script[src="/rapidoc-min.js"]')).toHaveLength(1);
  });
});
