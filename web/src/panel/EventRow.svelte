<script lang="ts">
  import { formatWhen } from '../lib/dates';
  import { describeEvent } from './events';
  import { t } from './i18n';
  import type { EventEntry } from './types';
  import Icon from './ui/Icon.svelte';

  // One journal entry: the mark of its tone, what happened, when, and the detail.
  let { event, now }: { event: EventEntry; now: number } = $props();
  const d = $derived(describeEvent(event));
  const at = $derived(Date.parse(event.at));
  const when = $derived.by(() => {
    const ago = now - at;
    if (ago >= 0 && ago < 3_600_000) return ago < 60_000 ? t('panel.ev.justNow') : t('panel.ev.minAgo', { n: Math.round(ago / 60_000) });
    return formatWhen(at);
  });
</script>

<div class={['ev', d.tone]}>
  <Icon name={d.icon} />
  <span class="w">{d.title}</span>
  <span class="ts">{when}</span>
  {#if d.detail}<span class="d">{d.detail}</span>{/if}
</div>
