<script lang="ts">
  import { t } from '../lib/i18n';
  import type { HistorySlot } from '../lib/model';

  // One stripe per 15 minutes: green passed, amber partly, red failed, grey no data.
  let { slots }: { slots: readonly HistorySlot[] } = $props();

  const cls = { ok: '', warn: 'w', bad: 'x', none: 'n' } as const;
</script>

<div class="hs" role="img" aria-label={t('hist.label')}>
  {#each slots as s (s.start)}<i class={cls[s.state]}></i>{/each}
</div>
