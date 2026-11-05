<script lang="ts">
  import { t } from '../lib/i18n';
  import { whyText } from '../lib/model';

  // What put a node in quarantine and what happens next, on the page of the node. Without a
  // suspicion of a block the sensors do not check it: it is only out of the queue.
  let { reasons, suspected = false }: { reasons: readonly string[]; suspected?: boolean | null } = $props();
  const why = $derived(whyText(reasons));
</script>

<section class="t qnote">
  <div class="th"><h2>{suspected ? t('quarantine.title') : t('outside.title')}</h2></div>
  <div class="tb-b">
    {#if reasons.length > 0}
      <p class="qwhy">{why[0].toUpperCase()}{why.slice(1)}.</p>
    {:else}
      <p class="qwhy">{t('quarantine.unknown')}</p>
    {/if}
    <p class="muted sm">{suspected ? t('quarantine.next') : t('outside.next')}</p>
  </div>
</section>
