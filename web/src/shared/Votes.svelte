<script lang="ts">
  import { t } from '../lib/i18n';
  import { sensorName } from '../lib/model';
  import type { LastRound } from '../lib/types';
  import { messages, type MessageKey } from '../strings.ru';
  import Icon from './Icon.svelte';

  // The sensors' votes of one round. A sensor that could not answer abstains: that is not a vote.
  let { votes }: { votes: LastRound['votes'] } = $props();

  const tone = { blocked: 'bad', clear: 'ok', abstain: 'off' } as const;
  const icon = { blocked: 'stop', clear: 'check', abstain: 'minus' } as const;

  // A reason the page knows is put in words; any other is shown as it came.
  const reasonText = (r: string) => {
    const key = `vote.reason.${r}`;
    return key in messages ? t(key as MessageKey) : r;
  };
</script>

<div class="votes">
  {#each votes as v (v.sensor)}
    <div class={['vote', v.vote === 'blocked' && 'bad']}>
      <span class="l">{sensorName(v.sensor)}</span>
      <span class="st {tone[v.vote]}"><Icon name={icon[v.vote]} />{t(`vote.${v.vote}`)}</span>
      {#if v.reason}<span class="faint xs">{reasonText(v.reason)}</span>{/if}
    </div>
  {/each}
</div>
