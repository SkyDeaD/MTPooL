<script module lang="ts">
  export type Section = 'stats' | 'dashboard' | 'links' | 'api';
</script>

<script lang="ts">
  import type { Snippet } from 'svelte';
  import { useNow } from '../lib/clock.svelte';
  import { t } from '../lib/i18n';
  import Logo from '../shared/Logo.svelte';
  import ThemeButton from '../shared/ThemeButton.svelte';
  import Toasts from '../shared/Toasts.svelte';

  // The frame of every public page: header with the sections, the page, the footer.
  let {
    active,
    updatedAt = null,
    stale = false,
    children,
  }: { active: Section | null; updatedAt?: number | null; stale?: boolean; children: Snippet } = $props();

  const now = useNow(1000);
  const tabs: { id: Section; href: string; label: string }[] = [
    { id: 'stats', href: '/', label: t('shell.tab.stats') },
    { id: 'dashboard', href: '/dashboard', label: t('shell.tab.dashboard') },
    { id: 'links', href: '/links', label: t('shell.tab.links') },
    { id: 'api', href: '/api', label: t('shell.tab.api') },
  ];
  const age = $derived.by(() => {
    if (updatedAt === null) return t('common.loading');
    const s = Math.max(0, Math.round((now.ms - updatedAt) / 1000));
    return s < 5 ? t('shell.age.now') : t('shell.age', { s });
  });
</script>

<header class="phd gut">
  <div class="in">
    <Logo href="/" />
    <nav class="ptabs" aria-label={t('shell.nav')}>
      {#each tabs as tab (tab.id)}
        <a href={tab.href} class={[active === tab.id && 'on']} aria-current={active === tab.id ? 'page' : undefined}>{tab.label}</a>
      {/each}
    </nav>
    <span class="sp"></span>
    <span class="live" title={stale ? t('common.stale') : undefined}><span class={['dot', stale && 'off']}></span>{age}</span>
    <ThemeButton />
    <a class="btn s" href="/panel/">{t('shell.panel')}</a>
  </div>
</header>
<main class="pm gut"><div class="view">{@render children()}</div></main>
<div class="gut" style="max-width:1240px;margin:auto"><p class="faint xs" style="padding-bottom:24px">{t('shell.footer')}</p></div>
<Toasts />
