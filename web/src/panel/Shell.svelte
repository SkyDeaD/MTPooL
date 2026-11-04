<script lang="ts">
  import { Popover } from 'bits-ui';
  import type { Snippet } from 'svelte';
  import type { IconName } from '../lib/icons';
  import { t } from '../lib/i18n';
  import type { MessageKey } from '../strings.ru';
  import Icon from '../shared/Icon.svelte';
  import Logo from '../shared/Logo.svelte';
  import ThemeButton from '../shared/ThemeButton.svelte';
  import { hrefOf, type Page, type Route } from './route.svelte';
  import Search from './Search.svelte';

  let {
    route,
    live,
    onlogout,
    children,
  }: { route: Route; live: boolean; onlogout: () => void; children: Snippet } = $props();

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
  const primary = items.slice(0, 4);
  const more = items.slice(4);

  const here = (page: Page) => (route.page === 'node' ? page === 'nodes' : route.page === page);
  let moreOpen = $state(false);
</script>

<div class="app">
  <nav class="rail" aria-label={t('panel.nav.label')}>
    <Logo href={hrefOf('overview')} />
    {#each items as item (item.page)}
      <a class={['nav', here(item.page) && 'on']} href={hrefOf(item.page)} aria-current={here(item.page) ? 'page' : undefined}>
        <Icon name={item.icon} />{t(item.label)}
      </a>
    {/each}
    <div class="sep"></div>
    <div class="foot">
      <button class="nav" type="button" onclick={onlogout}><Icon name="out" />{t('panel.logout')}</button>
    </div>
  </nav>

  <div class="main-col" style="min-width:0">
    <header class="mhead">
      <Logo href={hrefOf('overview')} />
      <span class="grow"></span>
      <ThemeButton />
    </header>
    <div class="main">
      <div class="top">
        <Search />
        <span class="grow"></span>
        <span class={['live-state', live && 'on']} role="status">
          <Icon name={live ? 'check' : 'clock'} />{live ? t('panel.live.on') : t('panel.live.off')}
        </span>
        <span class="desk-only"><ThemeButton /></span>
      </div>
      {@render children()}
    </div>
  </div>

  <nav class="bnav" aria-label={t('panel.nav.label')}>
    {#each primary as item (item.page)}
      <a class={[here(item.page) && 'on']} href={hrefOf(item.page)}><Icon name={item.icon} />{t(item.label)}</a>
    {/each}
    <Popover.Root bind:open={moreOpen}>
      <Popover.Trigger>
        <Icon name="more" />{t('panel.nav.more')}
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content class="pop" side="top" align="end" sideOffset={8}>
          {#each more as item (item.page)}
            <a class="nav" href={hrefOf(item.page)} onclick={() => (moreOpen = false)} style="display:flex;gap:8px;padding:8px 10px;color:var(--fg)">
              <Icon name={item.icon} />{t(item.label)}
            </a>
          {/each}
          <button type="button" onclick={onlogout}><Icon name="out" />{t('panel.logout')}</button>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  </nav>
</div>

<style>
  .desk-only {
    display: inline-flex;
  }
  @media (max-width: 860px) {
    .desk-only {
      display: none;
    }
  }
</style>
