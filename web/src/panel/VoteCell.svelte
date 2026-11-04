<script lang="ts">
  import { t } from '../lib/i18n';
  import Icon from '../shared/Icon.svelte';
  import { messages, type MessageKey } from '../strings.ru';
  import type { AdminVote } from './types';

  // One sensor's vote in one round. The measurements come from the sensor's own document, which
  // the panel does not control, so only what is really there is shown: the count of failed probes,
  // and the cities when the document lists them. Nothing is guessed.
  let { vote }: { vote: AdminVote | undefined } = $props();

  const tone = { blocked: 'bad', clear: 'ok', abstain: 'off' } as const;
  const icon = { blocked: 'stop', clear: 'check', abstain: 'minus' } as const;

  const reasonText = (r: string) => {
    const key = `vote.reason.${r}`;
    return r !== 'other' && key in messages ? t(key as MessageKey) : t('vote.reason.other');
  };

  interface City {
    city: string;
    ok: number;
    failed: number;
  }
  const evidence = $derived((vote?.evidence ?? null) as { probes?: unknown; failed?: unknown; cities?: unknown } | null);
  const probes = $derived(
    evidence && typeof evidence.probes === 'number' && typeof evidence.failed === 'number'
      ? { total: evidence.probes, failed: evidence.failed }
      : null,
  );
  const cities = $derived(
    evidence && Array.isArray(evidence.cities)
      ? (evidence.cities as City[]).filter((c) => c && typeof c.city === 'string' && typeof c.failed === 'number' && typeof c.ok === 'number')
      : [],
  );
</script>

{#if !vote}
  <span class="faint">—</span>
{:else}
  <span class="st {tone[vote.vote]}"><Icon name={icon[vote.vote]} />{t(`vote.${vote.vote}`)}</span>
  {#if vote.reason}<div class="faint xs">{reasonText(vote.reason)}</div>{/if}
  {#if probes}<div class="faint xs">{t('panel.rounds.probes', { failed: probes.failed, total: probes.total })}</div>{/if}
  {#if cities.length > 0}
    <div class="cities">
      {#each cities as c (c.city)}<div>{t('panel.rounds.city', { city: c.city, failed: c.failed, total: c.failed + c.ok })}</div>{/each}
    </div>
  {/if}
{/if}
