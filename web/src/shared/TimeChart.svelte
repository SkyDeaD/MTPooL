<script module lang="ts">
  export interface Bar {
    /** Start of the bucket, milliseconds. */
    at: number;
    value: number | null;
  }
</script>

<script lang="ts">
  import { formatDay, formatHour, formatSpan } from '../lib/dates';
  import { niceStep } from '../lib/chart';
  import { formatBytes } from '../lib/format';
  import { t, tp } from '../lib/i18n';

  // Bars over time: bans per bucket, or an average time in seconds. A bucket with no figure is
  // drawn empty and read as "no data".
  let { bars, unit, hourly }: { bars: readonly Bar[]; unit: 'count' | 'span' | 'bytes'; hourly: boolean } = $props();

  const max = $derived(Math.max(0, ...bars.map((b) => b.value ?? 0)));
  const scale = $derived.by(() => {
    if (unit === 'bytes') return niceStep(max / 4) || 1;
    if (unit === 'count') return max <= 4 ? 1 : max <= 10 ? 2 : 5;
    return max <= 7200 ? 1800 : max <= 43200 ? 10800 : max <= 172800 ? 43200 : 86400;
  });
  const top = $derived(Math.max(scale, Math.ceil(max / scale) * scale));
  const ticks = $derived.by(() => {
    const out: number[] = [];
    for (let v = 0; v <= top; v += scale) out.push(v);
    return out;
  });
  const every = $derived(ticks.length > 5 ? 2 : 1);
  const labelEvery = $derived(bars.length > 24 ? 5 : bars.length > 12 ? 3 : 1);

  const yLabel = (v: number) =>
    unit === 'count' ? String(v) : unit === 'bytes' ? formatBytes(v) : t('chart.hours', { n: Math.round(v / 3600) });
  const when = (at: number) => (hourly ? formatHour(at) : formatDay(at));
  const xLabel = (at: number) => (hourly ? formatHour(at).slice(0, 2) : formatDay(at));
  const text = (b: Bar) => {
    if (b.value === null || (unit !== 'count' && !b.value)) return `${when(b.at)}: ${t('chart.noData')}`;
    const v =
      unit === 'count' ? `${b.value} ${tp('unit.ban', b.value)}` : unit === 'bytes' ? formatBytes(b.value) : formatSpan(b.value);
    return `${when(b.at)}: ${v}`;
  };

  let tip = $state<{ text: string; x: number; y: number } | null>(null);
  function show(event: Event, b: Bar): void {
    const box = (event.currentTarget as HTMLElement).getBoundingClientRect();
    tip = { text: text(b), x: box.left + box.width / 2, y: box.top };
  }
</script>

<div class="ch">
  <div class="ya">
    {#each ticks.filter((_, i) => i % every === 0) as v (v)}
      <span style="bottom:{((100 * v) / top).toFixed(1)}%">{yLabel(v)}</span>
    {/each}
  </div>
  <div class="pl">
    {#each ticks.slice(1).filter((_, i) => (i + 1) % every === 0) as v (v)}
      <div class="gl" style="bottom:{((100 * v) / top).toFixed(1)}%"></div>
    {/each}
    <div class="bs">
      {#each bars as b, i (b.at)}
        <div
          class="bc"
          role="img"
          tabindex="0"
          data-v={b.value ? 1 : 0}
          aria-label={text(b)}
          onpointerenter={(e) => show(e, b)}
          onfocus={(e) => show(e, b)}
          onpointerleave={() => (tip = null)}
          onblur={() => (tip = null)}
        >
          <div class="bb" style="height:{(((b.value ?? 0) * 100) / top).toFixed(1)}%"></div>
        </div>
      {/each}
    </div>
  </div>
</div>
<div class="xa">
  {#each bars as b, i (b.at)}
    <span>{i % labelEvery === 0 || (i === bars.length - 1 && labelEvery === 1) ? xLabel(b.at) : ''}</span>
  {/each}
</div>
{#if tip}
  <div class="tip" style="left:{Math.max(8, tip.x - 60)}px;top:{Math.max(8, tip.y - 44)}px">{tip.text}</div>
{/if}
