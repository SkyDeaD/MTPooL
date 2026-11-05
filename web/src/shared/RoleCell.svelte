<script lang="ts">
  import { t } from '../lib/i18n';
  import { clock, secondsUntil, shortDomain } from '../lib/model';
  import type { PublicNode } from '../lib/types';
  import MasterTag from './MasterTag.svelte';

  let { node, now }: { node: PublicNode; now: number } = $props();

  const rotate = $derived(secondsUntil(node.rotates_at, now));
  const round = $derived(secondsUntil(node.next_round_at, now));
</script>

{#if node.is_master}
  <MasterTag domain={node.domains[0] ? shortDomain(node.domains[0]) : undefined} />
  {#if rotate !== null}<div class="nm-s">{t('node.role.rotates', { time: clock(rotate) })}</div>{/if}
{:else if node.state === 'quarantine' && !node.suspected}
  <div>{t('node.role.none')}</div>
{:else if node.state === 'quarantine'}
  <div>{t('node.role.outside')}</div>
  {#if round !== null}<div class="nm-s">{t('node.role.nextRound', { time: clock(round) })}</div>{/if}
{:else}
  <div>{t('node.role.queue', { n: node.queue_position ?? '?' })}</div>
  {#if node.queue_position === 1}<div class="nm-s">{t('node.role.nextMaster')}</div>{/if}
{/if}
