import { render as renderApp } from 'svelte/server';
import App from './App.svelte';

/** The markup of the public page, for scripts/prerender.ts. */
export function render(): { head: string; body: string } {
  return renderApp(App);
}
