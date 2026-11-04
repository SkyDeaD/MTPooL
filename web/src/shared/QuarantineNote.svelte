<script lang="ts">
  import { t } from '../lib/i18n';
  import { clock, whyText } from '../lib/model';

  // What put a node in quarantine and what happens next, on the page of the node.
  let { reasons, nextRound }: { reasons: readonly string[]; nextRound: number | null } = $props();
  const why = $derived(whyText(reasons));
</script>

<section class="t qnote">
  <div class="th"><h2>{t('quarantine.title')}</h2></div>
  <div class="tb-b">
    {#if reasons.length > 0}
      <p class="qwhy">{why[0].toUpperCase()}{why.slice(1)}.</p>
    {:else}
      <p class="qwhy">{t('quarantine.unknown')}</p>
    {/if}
    <p class="muted sm">
      {t('quarantine.next')}
      {#if nextRound !== null && nextRound > 0}{t('quarantine.nextRound', { time: clock(nextRound) })}{/if}
    </p>
  </div>
</section>
