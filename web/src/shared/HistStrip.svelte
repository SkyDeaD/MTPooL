<script lang="ts">
  import { t } from '../lib/i18n';
  import type { HistorySlot } from '../lib/model';

  // One stripe per 15 minutes: green passed, amber partly, red failed, grey no data. The legend
  // says so, because a colour alone tells nobody what it stands for.
  let { slots }: { slots: readonly HistorySlot[] } = $props();

  const cls = { ok: '', warn: 'w', bad: 'x', none: 'n' } as const;
  const LEGEND = ['ok', 'warn', 'bad', 'none'] as const;
</script>

<div class="hs" role="img" aria-label={t('hist.label')}>
  {#each slots as s (s.start)}<i class={cls[s.state]}></i>{/each}
</div>
<div class="hs-lg">
  {#each LEGEND as state (state)}
    <span><i class={['hs-k', cls[state]]}></i>{t(`hist.${state}`)}</span>
  {/each}
</div>
