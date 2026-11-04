<script lang="ts">
  import { useNow } from '../lib/clock.svelte';
  import { formatSpan } from '../lib/dates';
  import { t, tp } from '../lib/i18n';
  import { buildScheme, formatAvailability, groupNodes, poolCounts } from '../lib/model';
  import { useResource } from '../lib/resource.svelte';
  import type { ClientsRange, ClientsSeries, PublicDomain, PublicNode, StatsSummary } from '../lib/types';
  import ClientsChart from '../shared/ClientsChart.svelte';
  import Icon from '../shared/Icon.svelte';
  import NodeTable from '../shared/NodeTable.svelte';
  import PoolScheme from '../shared/PoolScheme.svelte';
  import PublicShell from './PublicShell.svelte';

  // Statistics of the pool: state, counters, clients over time, the scheme and the node table.
  let { fetchFn }: { fetchFn?: typeof fetch } = $props();

  let range = $state<ClientsRange>('10m');

  const nodes = useResource<PublicNode[]>(() => '/api/v1/nodes', fetchFn);
  const domains = useResource<PublicDomain[]>(() => '/api/v1/domains', fetchFn);
  const summary = useResource<StatsSummary>(() => '/api/v1/stats/summary?range=7d', fetchFn);
  const clients = useResource<ClientsSeries>(() => `/api/v1/stats/clients?range=${range}`, fetchFn);
  const clock = useNow(1000);

  const nodeHref = (id: string) => `/node/?id=${encodeURIComponent(id)}`;

  const groups = $derived(nodes.data ? groupNodes(nodes.data) : null);
  const counts = $derived(nodes.data ? poolCounts(nodes.data) : null);
  const scheme = $derived(nodes.data && domains.data ? buildScheme(domains.data, nodes.data) : null);
  const series = $derived(clients.data && clients.data.range === range ? clients.data : null);
  const empty = $derived(counts !== null && counts.shown === 0);
  const stale = $derived(nodes.stale || domains.stale || summary.stale || clients.stale);
  const updatedAt = $derived(nodes.updatedAt);

  const healthy = $derived(
    groups !== null &&
      groups.quarantine.length === 0 &&
      (scheme?.domains.every((d) => d.masters.length > 0) ?? true),
  );
  const heroText = $derived.by(() => {
    if (!groups || !counts) return '';
    const q = groups.quarantine.length;
    if (!healthy) {
      return q === 1
        ? t('stats.hero.changesOne')
        : t('stats.hero.changesMany', { n: q, word: tp('unit.node', q) });
    }
    return counts.shown === 1 ? t('stats.hero.okOne') : t('stats.hero.okText', { n: counts.shown });
  });
  const lastBans =$derived(summary.data?.kpi);
</script>

<PublicShell active="stats" {updatedAt} {stale}>
  <div class="stack">
    {#if stale}<p class="stale">{t('common.stale')}</p>{/if}

    {#if empty}
      <section class="t empty">
        <h2>{t('stats.empty.title')}</h2>
        <p>{t('stats.empty.text')}</p>
      </section>
    {:else if counts && groups}
      <section class="t hero">
        <div class="w">
          <span class={['ic', !healthy && 'warn']}><Icon name={healthy ? 'check' : 'alert'} lg /></span>
          <div>
            <h1>{healthy ? t('stats.hero.ok') : t('stats.hero.changes')}</h1>
            <p class="muted" style="margin-top:3px">{heroText}</p>
          </div>
        </div>
        <a class="btn pri" href="/links"><Icon name="send" />{t('stats.hero.connect')}</a>
      </section>

      <div class="kpis">
        <section class="t kpi" data-kpi="working">
          <span class="l">{t('stats.kpi.working')}</span>
          <span class="v">{counts.working}</span>
          <span class="s">{t('stats.kpi.workingOf', { n: counts.shown })}</span>
        </section>
        <section class="t kpi" data-kpi="availability">
          <span class="l">{t('stats.kpi.availability')}</span>
          <span class="v">{formatAvailability(counts.availabilityBp)}</span>
          <span class="s">{t('stats.kpi.availabilityNote')}</span>
        </section>
        <section class="t kpi" data-kpi="bans">
          <span class="l">{t('stats.kpi.bans')}</span>
          <span class="v">{lastBans ? lastBans.bans : '—'}</span>
          <a class="s" href="/dashboard">
            {lastBans?.avg_lifetime_sec ? t('stats.kpi.bansLife', { span: formatSpan(lastBans.avg_lifetime_sec) }) : t('stats.kpi.bansLink')}
          </a>
        </section>
      </div>
    {:else if !stale}
      <section class="t pad"><div class="sk" style="height:64px"></div></section>
      <div class="kpis">
        {#each [0, 1, 2] as i (i)}
          <section class="t kpi"><div class="sk" style="height:76px"></div></section>
        {/each}
      </div>
    {/if}

    {#if !empty}
      <ClientsChart {series} {range} onrange={(r) => (range = r)} />

      {#if scheme}<PoolScheme {scheme} href={nodeHref} now={clock.ms} />{/if}

      <section class="t">
        <div class="th">
          <h2>{t('stats.nodes.title')}</h2>
          <span class="sub">{t('stats.nodes.sub')}</span>
        </div>
        {#if groups}
          <NodeTable {groups} href={nodeHref} now={clock.ms} />
        {:else if !stale}
          <div class="tb-b"><div class="sk" style="height:120px"></div></div>
        {/if}
      </section>
    {/if}
  </div>
</PublicShell>
