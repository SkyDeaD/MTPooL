<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { t } from '../lib/i18n';
  import Toasts from '../shared/Toasts.svelte';
  import { createPanel, providePanel } from './context.svelte';
  import Journal from './Journal.svelte';
  import Login from './Login.svelte';
  import Masters from './Masters.svelte';
  import NodePage from './NodePage.svelte';
  import Nodes from './Nodes.svelte';
  import Overview from './Overview.svelte';
  import Owners from './Owners.svelte';
  import { current, startRouter } from './route.svelte';
  import Settings from './Settings.svelte';
  import Shell from './Shell.svelte';
  import Srmd from './Srmd.svelte';
  import { connectStream, type StreamState } from './stream';
  import Updates from './Updates.svelte';

  // The panel: a login, and behind it the pages. The route lives in the address after `#`, so it
  // is still there when the session ends and the reader signs in again.
  let {
    fetchFn = (...args: Parameters<typeof fetch>) => fetch(...args),
    eventSource = globalThis.EventSource ?? null,
  }: { fetchFn?: typeof fetch; eventSource?: typeof EventSource | null } = $props();

  let session = $state<'checking' | 'out' | 'in'>('checking');
  let expired = $state(false);
  let stream = $state<StreamState>('connecting');

  const panel = createPanel(untrack(() => fetchFn), () => {
    if (session === 'in') {
      expired = true;
      session = 'out';
    }
  });
  providePanel(panel);

  onMount(() => {
    const stop = startRouter();
    void panel.client
      .restoreSession()
      .then((live) => (session = live ? 'in' : 'out'))
      .catch(() => (session = 'out'));
    return stop;
  });

  // The live stream runs while there is a session. Whatever it reports is read again at once; the
  // pages poll every 30 seconds either way, so a lost stream costs only speed.
  $effect(() => {
    if (session !== 'in') return;
    return connectStream({
      source: eventSource,
      onState: (s) => (stream = s),
      onEvent: () => panel.refreshAll(),
      onResync: () => panel.refreshAll(),
    });
  });

  async function logout(): Promise<void> {
    try {
      await panel.client.logout();
    } finally {
      expired = false;
      session = 'out';
    }
  }
</script>

{#if session === 'checking'}
  <main class="login"><p class="muted">{t('common.loading')}</p></main>
{:else if session === 'out'}
  <Login {expired} onlogin={() => ((expired = false), (session = 'in'))} />
{:else}
  <Shell route={current.route} live={stream === 'live'} onlogout={logout}>
    {#key current.route.page}
      {#if current.route.page === 'overview'}
        <Overview />
      {:else if current.route.page === 'nodes'}
        <Nodes />
      {:else if current.route.page === 'node'}
        <NodePage id={current.route.arg ?? ''} />
      {:else if current.route.page === 'masters'}
        <Masters />
      {:else if current.route.page === 'srmd'}
        <Srmd />
      {:else if current.route.page === 'journal'}
        <Journal tab={current.route.arg ?? undefined} />
      {:else if current.route.page === 'settings'}
        <Settings />
      {:else if current.route.page === 'owners'}
        <Owners />
      {:else if current.route.page === 'updates'}
        <Updates />
      {/if}
    {/key}
  </Shell>
{/if}
<Toasts />
