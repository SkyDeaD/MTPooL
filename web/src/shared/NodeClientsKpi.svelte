<script lang="ts">
  import { formatBytes } from '../lib/format';
  import { t, tp } from '../lib/i18n';
  import type { PublicNode } from '../lib/types';
  import Sparkline from './Sparkline.svelte';

  /** The clients of one node in the pool and, apart from them, the owner's own users. */
  let { node, spark }: { node: PublicNode; spark: (number | null)[] } = $props();

  const other = $derived(node.other_clients ?? 0);
</script>

<section class="t kpi">
  <span class="l">{t('node.kpi.clients')}</span>
  <span class="v">{node.clients ?? '—'}</span>
  {#if other > 0}
    <span class="s" title={t('clients.otherHint')}>{t('clients.otherNode', { n: other, word: tp('unit.ownClient', other) })}</span>
  {/if}
  {#if node.other_traffic_bytes}
    <span class="s">{t('clients.otherTraffic', { size: formatBytes(node.other_traffic_bytes) })}</span>
  {/if}
  {#if spark.length > 1}<Sparkline values={spark} />{/if}
</section>
