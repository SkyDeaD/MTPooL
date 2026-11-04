import { render as renderApp } from 'svelte/server';
import ApiDocs from './ApiDocs.svelte';
import Dashboard from './Dashboard.svelte';
import Links from './Links.svelte';
import NodePage from './NodePage.svelte';
import Stats from './Stats.svelte';

export type PageName = 'stats' | 'dashboard' | 'links' | 'api' | 'node';

/** The markup of one public page, for scripts/prerender.ts. Data is not loaded: the pages start in the loading state. */
export function render(page: PageName): { head: string; body: string } {
  switch (page) {
    case 'stats':
      return renderApp(Stats);
    case 'dashboard':
      return renderApp(Dashboard);
    case 'links':
      return renderApp(Links);
    case 'api':
      return renderApp(ApiDocs);
    case 'node':
      return renderApp(NodePage, { props: { id: '' } });
  }
}
