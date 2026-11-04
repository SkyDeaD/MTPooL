<script lang="ts">
  import { t, type Key } from './i18n';
  import type { PublicDomain } from '../lib/types';
  import Icon from './ui/Icon.svelte';
  import { useData } from './data.svelte';
  import { usePolled } from './live.svelte';
  import { hrefOf, type Page } from './route.svelte';

  // Search over the nodes, the domains and the sections. The nodes are the shell's own list; the
  // domains are read when the field is first used. `/` jumps here from anywhere but a text field.
  const data = useData();
  let query = $state('');
  let open = $state(false);
  let used = $state(false);
  let active = $state(0);
  let input: HTMLInputElement | undefined = $state();
  const domains = usePolled<PublicDomain[]>(() => (used ? '/api/v1/domains' : null));

  const SECTIONS: { page: Page; label: Key }[] = [
    { page: 'overview', label: 'panel.nav.overview' },
    { page: 'nodes', label: 'panel.nav.nodes' },
    { page: 'masters', label: 'panel.nav.masters' },
    { page: 'srmd', label: 'panel.nav.srmd' },
    { page: 'journal', label: 'panel.nav.journal' },
    { page: 'settings', label: 'panel.nav.settings' },
    { page: 'owners', label: 'panel.nav.owners' },
    { page: 'updates', label: 'panel.nav.updates' },
  ];

  interface Hit {
    href: string;
    label: string;
    kind: string;
  }
  const found = $derived.by<Hit[]>(() => {
    const q = query.trim().toLowerCase();
    if (!q) return [];
    const nodes = (data.nodes.data ?? [])
      .filter((n) => n.id.toLowerCase().includes(q) || n.full_ip.includes(q))
      .map((n) => ({ href: hrefOf('node', n.id), label: n.id, kind: t('panel.search.node') }));
    const doms = (domains.data ?? [])
      .filter((d) => d.domain.includes(q))
      .map((d) => ({ href: hrefOf('masters'), label: d.domain, kind: t('panel.search.domain') }));
    const secs = SECTIONS.filter((s) => t(s.label).toLowerCase().includes(q)).map((s) => ({
      href: hrefOf(s.page),
      label: t(s.label),
      kind: t('panel.search.section'),
    }));
    return [...nodes, ...doms, ...secs].slice(0, 8);
  });

  function pick(hit: Hit): void {
    query = '';
    open = false;
    window.location.hash = hit.href;
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

<div class="srch" role="search">
  <Icon name="search" />
  <input
    class="inp"
    bind:this={input}
    bind:value={query}
    type="search"
    autocomplete="off"
    placeholder={t('panel.search.placeholder')}
    aria-label={t('panel.search.label')}
    oninput={() => (active = 0)}
    onfocus={() => {
      open = true;
      used = true;
    }}
    onblur={() => setTimeout(() => (open = false), 150)}
    onkeydown={(e) => {
      if (e.key === 'ArrowDown') active = Math.min(active + 1, found.length - 1);
      if (e.key === 'ArrowUp') active = Math.max(active - 1, 0);
      if (e.key === 'Enter' && found[active]) pick(found[active]);
      if (e.key === 'Escape') input?.blur();
    }}
  />
  <kbd aria-hidden="true">/</kbd>
  {#if open && query.trim()}
    <div class="res">
      {#each found as hit, i (hit.href + hit.label)}
        <a href={hit.href} class={[i === active && 'on']} onclick={() => pick(hit)}>
          <span>{hit.label}</span><span class="faint sm">{hit.kind}</span>
        </a>
      {:else}
        <div class="e">{t('panel.search.none')}</div>
      {/each}
    </div>
  {/if}
</div>
