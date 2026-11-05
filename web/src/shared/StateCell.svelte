<script lang="ts">
  import { t } from '../lib/i18n';
  import { whyText } from '../lib/model';
  import type { PublicNode } from '../lib/types';
  import Icon from './Icon.svelte';

  // The state of a node, and for a quarantine the reason it is in one. A quarantine without a
  // suspicion of a block is only "out of the queue": a port, metrics or a stale report.
  let {
    state,
    suspected = false,
    reasons = [],
  }: { state: PublicNode['state']; suspected?: boolean | null; reasons?: readonly string[] } = $props();
</script>

{#if state === 'quarantine'}
  <span class="st warn"><Icon name="alert" />{suspected ? t('node.state.quarantine') : t('node.state.outside')}</span>
  {#if reasons.length > 0}<div class="nm-s why">{whyText(reasons)}</div>{/if}
{:else if state === 'banned'}
  <span class="st bad"><Icon name="stop" />{t('node.state.banned')}</span>
{:else}
  <span class="st ok"><Icon name="check" />{t('node.state.healthy')}</span>
{/if}
