<script lang="ts">
  import { t, tp } from '../lib/i18n';

  /**
   * The number of clients with a bar relative to the largest one; a dash when unknown. Under it the
   * owner's own users, which the pool cannot use and does not count.
   */
  let { clients, max, other }: { clients: number | null | undefined; max: number; other?: number | null } = $props();

  const width = $derived(
    typeof clients === 'number' && max > 0 ? Math.round((100 * clients) / max) : 0,
  );
</script>

{#if typeof clients === 'number'}
  {clients}<span class="bar gr"><i style="width:{width}%"></i></span>
{:else}
  <span class="faint">—</span>
{/if}
{#if other}
  <span class="oth" title={t('clients.otherHint')}>{t('clients.other', { n: other, word: tp('unit.ownClient', other) })}</span>
{/if}
