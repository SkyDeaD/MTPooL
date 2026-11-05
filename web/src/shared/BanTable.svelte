<script lang="ts">
  import type { Snippet } from 'svelte';
  import { formatSpan, formatWhen } from '../lib/dates';
  import { t } from '../lib/i18n';
  import type { BanRecord } from '../lib/types';

  // The log of bans. `href` returns the node's page, or null when the node has none to open
  // (a ban removes its record); the table knows nothing of routes. `action` is the last cell of a
  // row, for the panel, where a ban can be lifted.
  let {
    rows,
    href,
    compact = false,
    action,
  }: {
    rows: readonly BanRecord[];
    href: (id: string) => string | null;
    compact?: boolean;
    action?: Snippet<[BanRecord]>;
  } = $props();

  function open(event: MouseEvent): void {
    if ((event.target as Element).closest('a')) return;
    (event.currentTarget as HTMLElement).querySelector<HTMLAnchorElement>('a')?.click();
  }
</script>

<div class="tw">
  <table class="tb list">
    <thead>
      <tr>
        <th>{t('bans.col.when')}</th>
        <th>{t('bans.col.node')}</th>
        <th>{t('bans.col.address')}</th>
        <th class="n">{t('bans.col.lived')}</th>
        {#if !compact}<th class="n">{t('bans.col.gap')}</th>{/if}
        {#if action}<th><span class="sr-only">{t('bans.col.action')}</span></th>{/if}
      </tr>
    </thead>
    <tbody>
      {#each rows as b (b.id)}
        {@const link = href(b.node_id)}
        <tr class={link && 'go'} onclick={link ? open : undefined}>
          <td class="full nowrap" data-l={t('bans.col.when')}>{formatWhen(Date.parse(b.at))}</td>
          <td class="nm nowrap" data-l={t('bans.col.node')}>
            {#if link}<a class="nm" href={link}>{b.node_id}</a>{:else}{b.node_id}{/if}
          </td>
          <td class="mono" data-l={t('bans.col.address')}>{b.ip}</td>
          <td class="n" data-l={t('bans.col.lived')}>{b.lifetime_sec == null ? '—' : formatSpan(b.lifetime_sec)}</td>
          {#if !compact}<td class="n" data-l={t('bans.col.gap')}>{b.gap_sec ? formatSpan(b.gap_sec) : t('bans.first')}</td>{/if}
          {#if action}<td class="n">{@render action(b)}</td>{/if}
        </tr>
      {/each}
    </tbody>
  </table>
</div>
