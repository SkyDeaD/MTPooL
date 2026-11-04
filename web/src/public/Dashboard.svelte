<script lang="ts">
  import { formatSpan } from '../lib/dates';
  import { t, tp } from '../lib/i18n';
  import { reasonLabel } from '../lib/model';
  import { useResource } from '../lib/resource.svelte';
  import type { BanRecord, PublicNode, StatsRange, StatsSeries, StatsSummary } from '../lib/types';
  import BanTable from '../shared/BanTable.svelte';
  import Seg from '../shared/Seg.svelte';
  import TimeChart, { type Bar } from '../shared/TimeChart.svelte';
  import PublicShell from './PublicShell.svelte';

  // How often the pool loses nodes and how long they live: three figures, a chart and the log.
  let { fetchFn }: { fetchFn?: typeof fetch } = $props();

  type Metric = 'bans' | 'gap' | 'life';
  let range = $state<StatsRange>('7d');
  let metric = $state<Metric>('bans');

  const summary = useResource<StatsSummary>(() => `/api/v1/stats/summary?range=${range}`, fetchFn);
  const series = useResource<StatsSeries>(() => `/api/v1/stats/series?range=${range}`, fetchFn);
  const bans = useResource<BanRecord[]>(() => '/api/v1/bans?limit=50', fetchFn);
  const nodes = useResource<PublicNode[]>(() => '/api/v1/nodes', fetchFn);

  const sum = $derived(summary.data && summary.data.range === range ? summary.data : null);
  const ser = $derived(series.data && series.data.range === range ? series.data : null);
  // A ban that was put back for re-checking is not a new ban.
  const rows = $derived(bans.data ? bans.data.filter((b) => !b.reverify) : null);
  const known = $derived(new Set(nodes.data?.map((n) => n.id) ?? []));
  const href = (id: string) => (known.has(id) ? `/node/?id=${encodeURIComponent(id)}` : null);

  const bars = $derived<Bar[]>(
    (ser?.buckets ?? []).map((b) => ({
      at: Date.parse(b.start_ts),
      value: metric === 'bans' ? b.bans : metric === 'gap' ? (b.avg_gap_sec ?? null) : (b.avg_lifetime_sec ?? null),
    })),
  );
  const stale = $derived(summary.stale || series.stale || bans.stale || nodes.stale);
  const reasons = $derived(
    sum ? Object.entries(sum.by_reason).sort((a, b) => b[1] - a[1]) : [],
  );
  const noBansToday = $derived(range === '24h' && sum !== null && sum.kpi.bans === 0);

  const rangeItems = (['24h', '7d', '30d'] as const).map((value) => ({ value, label: t(`dash.range.${value}`) }));
  const metricItems = (['bans', 'gap', 'life'] as const).map((value) => ({ value, label: t(`dash.metric.${value}`) }));
</script>

<PublicShell active="dashboard" updatedAt={summary.updatedAt} {stale}>
  <div class="ph">
    <h1>{t('dash.title')}</h1>
    <span class="sub">{t('dash.sub')}</span>
  </div>
  <div class="stack">
    {#if stale}<p class="stale">{t('common.stale')}</p>{/if}

    <div class="kpis">
      <section class="t kpi" style="--i:0">
        <span class="l">{t('dash.kpi.bans')}</span>
        <span class="v">{sum ? sum.kpi.bans : '—'}</span>
        <span class="s">{t(`dash.of.${range}`)}</span>
      </section>
      <section class="t kpi" style="--i:1">
        <span class="l">{t('dash.kpi.life')}</span>
        <span class="v">{sum?.kpi.avg_lifetime_sec ? formatSpan(sum.kpi.avg_lifetime_sec) : '—'}</span>
        <span class="s">{t('dash.kpi.lifeNote')}</span>
      </section>
      <section class="t kpi" style="--i:2">
        <span class="l">{t('dash.kpi.gap')}</span>
        <span class="v">{sum?.kpi.avg_gap_sec ? formatSpan(sum.kpi.avg_gap_sec) : '—'}</span>
        <span class="s">{t('dash.kpi.gapNote')}</span>
      </section>
    </div>

    <section class="t" style="--i:3">
      <div class="th">
        <h2>{t(`dash.chart.${metric}`)}</h2>
        <span class="sp"></span>
        <div class="row w">
          <Seg items={metricItems} value={metric} onchange={(v) => (metric = v)} label={t('dash.metric.label')} />
          <Seg items={rangeItems} value={range} onchange={(v) => (range = v)} label={t('dash.range.label')} />
        </div>
      </div>
      <div class="tb-b">
        {#if noBansToday}
          <div class="ch-empty">
            <div>
              <span>{t('dash.today.none')}</span>
              {#if rows?.[0]}
                <span class="faint sm">{t('dash.today.last', { node: rows[0].node_id })}</span>
              {/if}
            </div>
          </div>
        {:else if ser}
          {#if !ser.history_ok}<p class="faint sm">{t('dash.historyShort')}</p>{/if}
          <TimeChart {bars} unit={metric === 'bans' ? 'count' : 'span'} hourly={range === '24h'} />
        {:else if !series.stale}
          <div class="sk" style="height:180px"></div>
        {/if}
      </div>
    </section>

    {#if reasons.length > 0}
      <section class="t" style="--i:4">
        <div class="th">
          <h2>{t('dash.reasons.title')}</h2>
          <span class="sub">{t('dash.reasons.sub')}</span>
        </div>
        <div class="tb-b">
          <ul class="reasons">
            {#each reasons as [reason, n] (reason)}
              <li><span>{reasonLabel(reason)}</span><b>{n} {tp('unit.ban', n)}</b></li>
            {/each}
          </ul>
        </div>
      </section>
    {/if}

    <section class="t" style="--i:5">
      <div class="th">
        <h2>{t('dash.log.title')}</h2>
        {#if rows}<span class="sub">{rows.length} {tp('unit.record', rows.length)}</span>{/if}
      </div>
      {#if rows && rows.length > 0}
        <BanTable {rows} {href} />
      {:else if rows}
        <div class="tb-b"><p class="muted">{t('dash.log.empty')}</p></div>
      {:else if !bans.stale}
        <div class="tb-b"><div class="sk" style="height:120px"></div></div>
      {/if}
    </section>

    <p class="faint sm" style="max-width:70ch">{t('dash.traffic')}</p>
  </div>
</PublicShell>

<style>
  .reasons {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 6px;
  }
  .reasons li {
    display: flex;
    justify-content: space-between;
    gap: 12px;
  }
</style>
