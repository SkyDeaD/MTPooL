import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

// Two entries: the public pages (prerendered, see scripts/prerender.ts) and the panel, a single
// page app under /panel. The server answers /panel/* with panel/index.html.
export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // Fonts stay files (hashed, immutable), nothing is inlined into the CSS.
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    rollupOptions: {
      input: {
        index: 'index.html',
        panel: 'panel/index.html',
      },
    },
  },
  server: {
    // A registrar started with its defaults.
    proxy: { '/api': 'http://127.0.0.1:8080' },
  },
});
