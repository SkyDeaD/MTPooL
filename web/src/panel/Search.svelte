<script lang="ts">
  import { t } from '../lib/i18n';
  import Icon from '../shared/Icon.svelte';
  import { usePanel } from './context.svelte';
  import { go } from './route.svelte';
  import type { AdminNode } from './types';

  // Quick search over the nodes by name or address. The list is read when the field is first used,
  // so the page itself asks nothing extra. `/` jumps here from anywhere but a text field.
  const panel = usePanel();
  let nodes = $state<AdminNode[] | null>(null);
  let query = $state('');
  let open = $state(false);
  let input: HTMLInputElement | undefined = $state();

  async function load(): Promise<void> {
    if (nodes) return;
    try {
      nodes = await panel.client.get('/api/v1/admin/nodes');
    } catch {
      nodes = [];
    }
  }

  const found = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (!q || !nodes) return [];
    return nodes.filter((n) => n.state !== 'banned' && (n.id.toLowerCase().includes(q) || n.full_ip.includes(q))).slice(0, 8);
  });

  function pick(id: string): void {
    query = '';
    open = false;
    go('node', id);
  }

  function hotkey(event: KeyboardEvent): void {
    const tag = (event.target as HTMLElement | null)?.tagName;
    if (event.key === '/' && tag !== 'INPUT' && tag !== 'TEXTAREA' && tag !== 'SELECT') {
      event.preventDefault();
      input?.focus();
    }
  }
</script>

<svelte:window onkeydown={hotkey} />

<div class="srch">
  <Icon name="search" />
  <input
    class="inp"
    bind:this={input}
    bind:value={query}
    type="search"
    placeholder={t('panel.search.placeholder')}
    aria-label={t('panel.search.placeholder')}
    onfocus={() => {
      open = true;
      void load();
    }}
    onblur={() => setTimeout(() => (open = false), 150)}
    onkeydown={(e) => {
      if (e.key === 'Enter' && found[0]) pick(found[0].id);
      if (e.key === 'Escape') input?.blur();
    }}
  />
  <kbd aria-hidden="true">/</kbd>
  {#if open && query.trim()}
    <div class="res">
      {#each found as n (n.id)}
        <a href={`#/nodes/${encodeURIComponent(n.id)}`} onclick={() => pick(n.id)}>
          <span class="nm">{n.id}</span><span class="mono muted">{n.ip}</span>
        </a>
      {:else}
        <div class="e">{t('panel.search.none')}</div>
      {/each}
    </div>
  {/if}
</div>
