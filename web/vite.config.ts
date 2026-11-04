import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';
import { devApi } from './scripts/dev-api.ts';

// Entries: the public pages (prerendered, see scripts/prerender.ts) and the panel, a single page
// app under /panel, built in a second pass. The server answers /panel/* with panel/index.html. Each public page is its
// own entry, so a page loads only the code it uses.
//
// `npm run dev` serves /api/v1/* from tests/fixtures (scripts/dev-api.ts), so the pages show data
// without a registrar. `npm run dev:real` proxies /api/v1 to a registrar on 127.0.0.1:8080 instead.
export default defineConfig(({ command }) => {
  // The panel is built on its own (second pass, MTPOOL_PANEL=1). In one build the bundler moves what
  // the panel and a public page both use into a chunk every public page preloads, and the budget grows.
  const panelPass = Boolean(process.env.MTPOOL_PANEL);
  const mock = command === 'serve' && !process.env.VITEST && !process.env.MTPOOL_REAL_API;
  return {
    plugins: [svelte(), ...(mock ? [devApi()] : [])],
    // Components are tested in jsdom, with the browser build of Svelte.
    resolve: { conditions: process.env.VITEST ? ['browser'] : undefined },
    test: { include: ['tests/**/*.test.ts'], setupFiles: ['tests/setup.ts'] },
    build: {
      outDir: 'dist',
      emptyOutDir: !panelPass,
      // Fonts stay files (hashed, immutable), nothing is inlined into the CSS.
      assetsInlineLimit: 0,
      modulePreload: { polyfill: false },
      rollupOptions: {
        input: (panelPass
          ? { panel: 'panel/index.html' }
          : {
              index: 'index.html',
              dashboard: 'dashboard/index.html',
              links: 'links/index.html',
              api: 'api/index.html',
              node: 'node/index.html',
            }) as Record<string, string>,
      },
    },
    server: {
      // A registrar started with its defaults.
      // Only what the registrar serves: /api/ itself is a page of this project.
      proxy: {
        '/api/v1': 'http://127.0.0.1:8080',
        '/install.sh': 'http://127.0.0.1:8080',
        '/releases': 'http://127.0.0.1:8080',
      },
    },
  };
});
