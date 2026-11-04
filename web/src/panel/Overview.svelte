<script lang="ts">
  import { LIVE_STEP_MS, mergeLive, withLiveClients } from '../lib/clients-live';
  import { useClientsLive } from '../lib/clients-live.svelte';
  import { useNow } from '../lib/clock.svelte';
  import { formatSpan, formatWhen } from '../lib/dates';
  import { buildScheme, otherClientsTotal, secondsUntil, whyText } from '../lib/model';
  import type { BanRecord, ClientsRange, ClientsSeries, PublicDomain, StatsSeries } from '../lib/types';
  import BanTable from '../shared/BanTable.svelte';
  import ClientsChart from '../shared/ClientsChart.svelte';
  import PoolScheme from '../shared/PoolScheme.svelte';
  import TimeChart, { type Bar } from '../shared/TimeChart.svelte';
  import { act } from './act';
  import AddNode from './AddNode.svelte';
  import { usePanel } from './context.svelte';
  import { useData } from './data.svelte';
  import EventRow from './EventRow.svelte';
  import { t, tp } from './i18n';
  import { usePolled } from './live.svelte';
  import { panelGroups } from './nodes';
  import { hrefOf } from './route.svelte';
  import type { AdminNode, EventEntry } from './types';
  import { sensorKey } from './types';
  import Icon from './ui/Icon.svelte';

  // The pool at a glance: what needs a person, the main numbers, clients, the scheme, bans, events.
  const panel = usePanel();
  const data = useData();
  const clock = useNow(1000);
  const live = useClientsLive();
  let range = $state<ClientsRange>('10m');
  let adding = $state(false);

  const domains = usePolled<PublicDomain[]>(() => '/api/v1/domains');
  const clients = usePolled<ClientsSeries>(() => `/api/v1/stats/clients?range=${range}`);
  const series = usePolled<StatsSeries>(() => '/api/v1/stats/series?range=7d');
  const bans = usePolled<BanRecord[]>(() => '/api/v1/bans?limit=50');
  const events = usePolled<EventEntry[]>(() => '/api/v1/admin/events');

  const nodes = $derived(data.nodes.data);
  const status = $derived(data.status.data);
  const groups = $derived(nodes ? panelGroups(nodes) : null);
  const pool = $derived(groups ? groups.masters.length + groups.queue.length + groups.quarantine.length : 0);
  const working = $derived(groups ? groups.masters.length + groups.queue.length : 0);
  const trouble = $derived(groups ? groups.quarantine.length : 0);

  const hosts = $derived((domains.data ?? []).filter((d) => !d.folded_into));
  const withMaster = $derived(hosts.filter((d) => d.masters.length > 0).length);
  const foldedCount = $derived((domains.data ?? []).length - hosts.length);
  const scheme = $derived(nodes && domains.data ? buildScheme(domains.data, nodes) : null);
  const ser = $derived(series.data && series.data.range === '7d' ? series.data : null);
  // Average node life over the week, weighted by how many bans each day had.
  const avgLife = $derived.by(() => {
    const days = (ser?.buckets ?? []).filter((b) => b.avg_lifetime_sec && b.bans > 0);
    const bans = days.reduce((sum, b) => sum + b.bans, 0);
    return bans ? Math.round(days.reduce((sum, b) => sum + (b.avg_lifetime_sec ?? 0) * b.bans, 0) / bans) : 0;
  });
  const bars = $derived<Bar[]>((ser?.buckets ?? []).map((b) => ({ at: Date.parse(b.start_ts), value: b.bans })));
  const chartSeries = $derived(clients.data && clients.data.range === range ? mergeLive(clients.data, live.ticks) : null);
  const recentBans = $derived((bans.data ?? []).filter((b) => !b.reverify).slice(0, 5));
  const known = $derived(new Set((nodes ?? []).map((n) => n.id)));
  const banHref = (id: string) => (known.has(id) ? hrefOf('node', id) : null);

  const dnsBroken = $derived(!!status?.dns.last_error);
  const silent = $derived(status ? status.sensors.filter((s) => !s.configured) : []);
  const attention = $derived((groups?.quarantine.length ?? 0) + (dnsBroken ? 1 : 0) + silent.length);
  const calm = $derived(groups !== null && status !== null && attention === 0);

  const unban = (id: string) =>
    act(panel, () => panel.client.post('/api/v1/admin/nodes/{id}/unban', { params: { id } }), t('panel.node.unbanned', { id }));

  function quarantineText(n: AdminNode): string {
    const blocked = n.last_round?.votes.filter((v) => v.vote === 'blocked').length ?? 0;
    const left = secondsUntil(n.next_round_at, clock.ms);
    const next = left !== null && left > 0 ? t('panel.overview.nextRound', { span: formatSpan(left) }) : '';
    const failed = whyText(n.quarantine_reasons?.length ? n.quarantine_reasons : n.failed);
    const why = failed && `${failed[0].toUpperCase()}${failed.slice(1)}.`;
    return [why, blocked === 1 ? t('panel.overview.oneVote') : '', next].filter(Boolean).join(' ');
  }
  const stale = $derived(data.nodes.stale || data.status.stale || domains.stale);
</script>

<div class="view stack">
  <header class="ph">
    <h1>{t('panel.overview.title')}</h1>
    <span class="sp"></span>
    <button class="btn pri" type="button" onclick={() => (adding = true)}><Icon name="plus" />{t('panel.nodes.add')}</button>
  </header>
  {#if stale}<p class="note" role="status">{t('common.stale')}</p>{/if}

  {#if calm}
    <section class="t"><div class="calm"><Icon name="check" /><span>{t('panel.overview.calm')}</span></div></section>
  {:else if groups && status}
    <section class={['t', dnsBroken ? 'bad-e' : 'warn-e']}>
      <div class="th"><h2>{t('panel.overview.attention', { n: attention })}</h2></div>
      <div class="attn">
        {#each groups.quarantine as n (n.id)}
          <div class="it">
            <Icon name="alert" lg />
            <div class="tx">
              <div class="b">{t('panel.overview.quarantined', { id: n.id })}</div>
              <div class="muted sm">{quarantineText(n)}</div>
            </div>
            <a class="btn s" href={hrefOf('node', n.id)}>{t('panel.overview.openNode')}</a>
          </div>
        {/each}
        {#if dnsBroken}
          <div class="it bad">
            <Icon name="stop" lg />
            <div class="tx">
              <div class="b">{t('panel.overview.dnsFails')}</div>
              <div class="muted sm">{t('panel.overview.dnsFailsText')}</div>
            </div>
            <a class="btn s" href={hrefOf('masters')}>{t('panel.overview.openMasters')}</a>
          </div>
        {/if}
        {#each silent as s (s.name)}
          <div class="it">
            <Icon name="alert" lg />
            <div class="tx">
              <div class="b">{t('panel.overview.sensorOff', { name: t(`sensor.${sensorKey(s.name)}` as 'sensor.globalping') })}</div>
              <div class="muted sm">{t('panel.overview.sensorOffText')}</div>
            </div>
            <a class="btn s" href={hrefOf('settings')}>{t('panel.overview.openSettings')}</a>
          </div>
        {/each}
      </div>
    </section>
  {/if}

  <div class="kpis">
    <section class="t kpi">
      <span class="l">{t('panel.overview.domains')}</span>
      <span class="v">{domains.data ? t('panel.overview.of', { a: withMaster, b: hosts.length }) : '—'}</span>
      <span class="s">{foldedCount === 0 ? t('panel.overview.allUp') : t('panel.overview.foldedMore', { n: foldedCount })}</span>
    </section>
    <section class="t kpi">
      <span class="l">{t('panel.overview.nodes')}</span>
      <span class="v">{groups ? t('panel.overview.of', { a: working, b: pool }) : '—'}</span>
      <span class="s">
        {#if groups}
          {t('panel.overview.nodesNote', { m: groups.masters.length, q: groups.queue.length })}{trouble > 0 ? t('panel.overview.nodesTrouble', { n: trouble }) : ''}
        {/if}
      </span>
    </section>
    <section class="t kpi">
      <span class="l">{t('panel.overview.changes')}</span>
      <span class="v">{status?.counts.master_changes_24h ?? '—'}</span>
      <span class="s">{status ? (status.counts.master_changes_24h > 0 ? t('panel.overview.changesNote', { span: formatSpan(Math.round(86400 / status.counts.master_changes_24h)) }) : t('panel.overview.changesNone')) : ''}</span>
    </section>
    <section class="t kpi">
      <span class="l">{t('panel.overview.bans')}</span>
      <span class="v">{status?.counts.bans_7d ?? '—'}</span>
      <span class="s">{avgLife ? t('panel.overview.bansNote', { span: formatSpan(avgLife) }) : status?.counts.bans_7d === 0 ? t('panel.overview.bansNone') : ''}</span>
    </section>
    <section class="t kpi">
      <span class="l">{t('panel.overview.dnsDay')}</span>
      <span class="v">{status?.dns.applied_24h ?? '—'} <small>{t('panel.overview.dnsOk')}</small></span>
      <span class="s">{status ? (status.dns.failed_24h > 0 ? t('panel.overview.dnsErrors', { n: status.dns.failed_24h }) : t('panel.overview.noErrors')) : ''}</span>
    </section>
  </div>

  <ClientsChart series={chartSeries} {range} onrange={(r) => (range = r)} delayMs={live.connected ? LIVE_STEP_MS * 1.5 : undefined} other={otherClientsTotal(withLiveClients(nodes ?? [], [], live.last).nodes)} />

  {#if scheme}<PoolScheme {scheme} href={(id) => hrefOf('node', id)} now={clock.ms} />{/if}

  <div class="cols c84">
    <section class="t">
      <div class="th">
        <h2>{t('panel.overview.bansChart')}</h2>
        <span class="sub">{t('panel.overview.bansChartSub')}</span>
      </div>
      <div class="tb-b">
        {#if ser}
          <TimeChart {bars} unit="count" hourly={false} />
        {:else}
          <div class="sk" style="height:180px"></div>
        {/if}
      </div>
    </section>
    <section class="t">
      <div class="th"><h2>{t('panel.overview.services')}</h2></div>
      <div class="svc">
        {#each status?.sensors ?? [] as s (s.name)}
          <div class="r">
            <span>{t(`sensor.${sensorKey(s.name)}` as 'sensor.globalping')}</span>
            <span class={['st', s.configured ? 'ok' : 'warn']}>{s.configured ? t('panel.overview.on') : t('panel.overview.off')}</span>
            <span class="faint sm">{s.last_answer_at ? formatWhen(Date.parse(s.last_answer_at)) : '—'}</span>
          </div>
        {/each}
        {#if status}
          <div class="r">
            <span>{t('panel.overview.cloudflare')}</span>
            <span class={['st', status.dns.token_set && !status.dns.last_error ? 'ok' : 'warn']}>
              {status.dns.last_error ? t('panel.overview.cfError') : status.dns.token_set ? t('panel.overview.on') : t('panel.overview.off')}
            </span>
            <span class="faint sm">{status.dns.last_applied_at ? formatWhen(Date.parse(status.dns.last_applied_at)) : '—'}</span>
          </div>
        {/if}
      </div>
    </section>
  </div>

  <div class="cols c75">
    <section class="t">
      <div class="th">
        <h2>{t('panel.overview.lastBans')}</h2>
        <span class="sp"></span>
        <a href="/dashboard/">{t('panel.overview.allBans')}</a>
      </div>
      {#if recentBans.length > 0}
        <BanTable rows={recentBans} href={banHref} compact>
          {#snippet action(ban)}
            <button class="btn s" type="button" onclick={() => unban(ban.node_id)}>{t('panel.node.unban')}</button>
          {/snippet}
        </BanTable>
      {:else}
        <div class="tb-b"><p class="muted">{t('dash.log.empty')}</p></div>
      {/if}
    </section>
    <section class="t">
      <div class="th">
        <h2>{t('panel.overview.events')}</h2>
        <span class="sp"></span>
        <a href={hrefOf('journal')}>{t('panel.overview.allEvents')}</a>
      </div>
      {#each (events.data ?? []).slice(0, 6) as e (e.id)}
        <EventRow event={e} now={clock.ms} />
      {:else}
        <div class="tb-b"><p class="muted">{t('panel.journal.none')}</p></div>
      {/each}
    </section>
  </div>
</div>

<AddNode bind:open={adding} />

<style>
  .svc {
    display: grid;
  }
  .svc .r {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto auto;
    gap: 10px;
    align-items: center;
    padding: 10px 16px;
    border-top: 1px solid var(--line);
  }
  .svc .r:first-child {
    border-top: 0;
  }
</style>
