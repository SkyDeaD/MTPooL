<script lang="ts">
  import type { Snippet } from 'svelte';
  import { currentTheme, toggleTheme, type Theme } from '../lib/theme';
  import Logo from '../shared/Logo.svelte';
  import ThemeButton from '../shared/ThemeButton.svelte';
  import { provideData } from './data.svelte';
  import type { IconName } from './icons';
  import { t } from './i18n';
  import type { Key as MessageKey } from './i18n';
  import Confirm from './ui/Confirm.svelte';
  import Icon from './ui/Icon.svelte';
  import Modal from './ui/Modal.svelte';
  import { guard, hrefOf, leave, type Page, type Route } from './route.svelte';
  import Search from './Search.svelte';

  let {
    route,
    live,
    onlogout,
    children,
  }: { route: Route; live: boolean; onlogout: () => void; children: Snippet } = $props();

  const data = provideData();

  const items: { page: Page; icon: IconName; label: MessageKey }[] = [
    { page: 'overview', icon: 'grid', label: 'panel.nav.overview' },
    { page: 'nodes', icon: 'server', label: 'panel.nav.nodes' },
    { page: 'masters', icon: 'globe', label: 'panel.nav.masters' },
    { page: 'srmd', icon: 'layers', label: 'panel.nav.srmd' },
    { page: 'journal', icon: 'list', label: 'panel.nav.journal' },
    { page: 'settings', icon: 'sliders', label: 'panel.nav.settings' },
    { page: 'owners', icon: 'users', label: 'panel.nav.owners' },
    { page: 'updates', icon: 'up', label: 'panel.nav.updates' },
  ];
  // The bottom bar of a phone has room for four; the rest wait behind «Ещё».
  const bar: { page: Page; icon: IconName; label: MessageKey }[] = [
    { page: 'overview', icon: 'grid', label: 'panel.nav.overview' },
    { page: 'nodes', icon: 'server', label: 'panel.nav.nodes' },
    { page: 'masters', icon: 'globe', label: 'panel.nav.mastersShort' },
    { page: 'journal', icon: 'list', label: 'panel.nav.journal' },
  ];
  const more = items.filter((i) => !bar.some((b) => b.page === i.page));

  const here = (page: Page) => (route.page === 'node' ? page === 'nodes' : route.page === page);
  const moreHere = $derived(more.some((i) => here(i.page)));
  let moreOpen = $state(false);

  // What the counters in the rail say: the nodes the pool holds, and those in trouble.
  const pool = $derived((data.nodes.data ?? []).filter((n) => n.state !== 'banned' && n.state !== 'pruned'));
  const trouble = $derived(pool.filter((n) => n.state === 'quarantine').length);
  const running = $derived((data.rollouts.data ?? []).some((r) => ['canary', 'observing', 'batches', 'paused'].includes(r.phase)));
  function counter(page: Page): { text: string; bad: boolean } | null {
    if (page === 'nodes' && data.nodes.data) {
      return trouble > 0 ? { text: `${trouble} из ${pool.length}`, bad: true } : { text: String(pool.length), bad: false };
    }
    if (page === 'updates' && running) return { text: '1', bad: false };
    return null;
  }

  let theme = $state<Theme>('light');
  $effect(() => {
    theme = currentTheme();
  });
</script>

<div class="app">
  <aside class="rail" aria-label={t('panel.nav.label')}>
    <Logo href={hrefOf('overview')} />
    <nav style="display:grid;gap:2px">
      {#each items as item (item.page)}
        {@const c = counter(item.page)}
        <a class={['nav', here(item.page) && 'on']} href={hrefOf(item.page)} aria-current={here(item.page) ? 'page' : undefined}>
          <Icon name={item.icon} /><span>{t(item.label)}</span>
          {#if c}<span class={['cnt', c.bad && 'bad']}>{c.text}</span>{/if}
        </a>
      {/each}
    </nav>
    <div class="sep"></div>
    <div class="foot">
      <a class="nav" href="/"><Icon name="eye" /><span>{t('panel.nav.public')}</span></a>
      <button class="nav" type="button" onclick={() => (theme = toggleTheme())}>
        <Icon name={theme === 'dark' ? 'sun' : 'moon'} /><span>{theme === 'dark' ? t('panel.theme.light') : t('panel.theme.dark')}</span>
      </button>
      <button class="nav" type="button" onclick={onlogout}><Icon name="out" /><span>{t('panel.logout')}</span></button>
    </div>
  </aside>

  <div class="main-col" style="min-width:0">
    <header class="mhead">
      <Logo href={hrefOf('overview')} />
      <span class="grow"></span>
      <span class={['live-state', live && 'on']}><span class="dot" aria-hidden="true"></span></span>
      <ThemeButton />
    </header>
    <main class="main">
      <div class="top">
        <Search />
        <span class="grow"></span>
        <span class={['live-state', live && 'on']} role="status">
          <Icon name={live ? 'check' : 'clock'} />{live ? t('panel.live.on') : t('panel.live.off')}
        </span>
      </div>
      {@render children()}
    </main>
  </div>

  <nav class="bnav" aria-label={t('panel.nav.label')}>
    {#each bar as item (item.page)}
      <a class={[here(item.page) && 'on']} href={hrefOf(item.page)}><Icon name={item.icon} /><span>{t(item.label)}</span></a>
    {/each}
    <button type="button" class={[moreHere && 'on']} onclick={() => (moreOpen = true)}><Icon name="more" /><span>{t('panel.nav.more')}</span></button>
  </nav>
</div>

<Modal bind:open={moreOpen} title={t('panel.nav.moreTitle')}>
  <div style="display:grid;gap:2px">
    {#each more as item (item.page)}
      <a class="btn mrow-link" href={hrefOf(item.page)} onclick={() => (moreOpen = false)}><Icon name={item.icon} />{t(item.label)}</a>
    {/each}
    <a class="btn mrow-link" href="/"><Icon name="eye" />{t('panel.nav.public')}</a>
    <button class="btn mrow-link" type="button" onclick={onlogout}><Icon name="out" />{t('panel.logout')}</button>
  </div>
</Modal>

<Confirm
  bind:open={() => guard.pending !== null, (v) => { if (!v) guard.pending = null; }}
  title={t('panel.leave.title')}
  text={t('panel.leave.text')}
  keep={t('panel.leave.keep')}
  confirm={t('panel.leave.confirm')}
  onconfirm={leave}
/>
