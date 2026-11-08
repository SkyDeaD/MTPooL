<script lang="ts">
  import { withLiveClients } from '../lib/clients-live';
  import { useClientsLive } from '../lib/clients-live.svelte';
  import { useNow } from '../lib/clock.svelte';
  import { formatWhen } from '../lib/dates';
  import { t, tp } from './i18n';
  import { clock, ENGINE_NAMES, formatAvailability, historySlots, secondsUntil, shortDomain, whyText } from '../lib/model';
  import type { NodeHistory } from '../lib/types';
  import NodeClientsKpi from '../shared/NodeClientsKpi.svelte';
  import StateCell from '../shared/StateCell.svelte';
  import Votes from '../shared/Votes.svelte';
  import EventRow from './EventRow.svelte';
  import HistStrip from '../shared/HistStrip.svelte';
  import Icon from './ui/Icon.svelte';
  import MasterTag from '../shared/MasterTag.svelte';
  import QuarantineNote from '../shared/QuarantineNote.svelte';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import { usePolled } from './live.svelte';
  import RetireDialog from './RetireDialog.svelte';
  import { hrefOf } from './route.svelte';
  import type { AdminNodeDetail, EventEntry } from './types';
  import VoteCell from './VoteCell.svelte';

  // One node for the admin: the state, who owns it, and every round of checks with each sensor's
  // vote and what it measured. A banned node has no page: it leaves the pool with the ban, and is unbanned from the ban history.
  let { id }: { id: string } = $props();

  const panel = usePanel();
  const node = usePolled<AdminNodeDetail>(() => `/api/v1/admin/nodes/${encodeURIComponent(id)}`);
  const history = usePolled<NodeHistory>(() => `/api/v1/nodes/${encodeURIComponent(id)}/history`);
  const events = usePolled<EventEntry[]>(() => `/api/v1/admin/events?node=${encodeURIComponent(id)}`);
  const live = useClientsLive(() => {
    node.refresh();
    history.refresh();
    events.refresh();
  });
  const clock1 = useNow(1000);
  let retiring = $state<string | null>(null);

  const missing = $derived(node.data === null && node.status === 404);
  const slots = $derived(history.data ? historySlots(history.data, clock1.ms) : null);
  const rotate = $derived(node.data ? secondsUntil(node.data.rotates_at, clock1.ms) : null);
  const spark = $derived(slots ? slots.map((s) => s.clients) : []);
  const days = $derived(node.data ? Math.max(0, Math.floor((clock1.ms - Date.parse(node.data.registered_at)) / 86_400_000)) : 0);
  const round = $derived(node.data ? secondsUntil(node.data.next_round_at, clock1.ms) : null);
  const mine = $derived((events.data ?? []).filter((e) => e.node === id).slice(0, 8));
  const SENSORS = $derived<string[]>([
    'globalping',
    'check_host',
    ...((node.data?.rounds ?? []).some((r) => r.votes.some((v) => v.sensor === 'cheburcheck')) ? ['cheburcheck'] : []),
    'red_flag',
  ]);

  const recheck = () =>
    act(panel, () => panel.client.post('/api/v1/admin/nodes/{id}/recheck', { params: { id } }), t('panel.nodes.rechecked', { id }));
</script>

<div class="view stack">
  <a class="row sm" href={hrefOf('nodes')} style="width:max-content"><Icon name="back" />{t('panel.node.all')}</a>

  {#if missing}
    <div class="t pad">
      <h1 class="b" style="font-size:16px">{t('node.missing.title')}</h1>
      <p class="muted">{t('node.missing.text')} <a href={hrefOf('nodes')}>{t('node.missing.link')}</a>.</p>
    </div>
  {:else}
    {#if node.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}
    {#if node.data?.port_filter}<p class="note" role="status">{t('why.port_filter')}</p>{/if}
    <div class="ph" style="margin:0">
      <h1>{id}</h1>
      {#if node.data}
        {@const n = node.data}
        <StateCell state={n.state} suspected={n.suspected} />
        {#if n.is_master}<MasterTag domain={n.domains[0] ? shortDomain(n.domains[0]) : undefined} />{/if}
        <span class="sub"><span class="mono">{n.full_ip}:{n.port}</span>, {t('node.engine', { name: ENGINE_NAMES[n.engine] ?? n.engine })}, <a href={`/node/?id=${encodeURIComponent(id)}`}>{t('panel.node.public')}</a></span>
        <span class="sp"></span>
        <div class="row-actions">
          {#if n.state !== 'pruned'}
            <button class="btn" type="button" onclick={recheck}>{t('panel.node.recheck')}</button>
          {/if}
          {#if n.state !== 'pruned'}
            <button class="btn dng" type="button" onclick={() => (retiring = id)}>{t('panel.nodes.retire')}</button>
          {/if}
        </div>
      {/if}
    </div>

    {#if node.data}
      {@const n = node.data}
      <div class="kpis">
        <section class="t kpi">
          <span class="l">{t('node.kpi.availability')}</span>
          <span class="v">{formatAvailability(n.availability_bp)}</span>
          <span class="s">{t(days < 30 ? 'node.kpi.availabilityYoung' : 'node.kpi.availabilityNote')}</span>
        </section>
        <NodeClientsKpi node={withLiveClients([n], [], live.last).nodes[0]} {spark} />
        <section class={['t kpi', n.is_master && 'mk']}>
          <span class="l">{t('node.kpi.role')}</span>
          {#if n.is_master}
            <span class="v">{t('node.role.master')}</span>
            <span class="s">
              {#if n.domains[0]}<span class="mono">{shortDomain(n.domains[0])}</span>{/if}{#if rotate !== null}, {t('node.role.rotates', { time: clock(rotate) })}{/if}
            </span>
          {:else if n.state === 'healthy'}
            <span class="v">{n.queue_position ? t('node.kpi.queuePlace', { n: n.queue_position }) : t('node.kpi.queue')}</span>
            <span class="s">{t('node.kpi.queueText')}</span>
          {:else}
            <span class="v">{t('node.role.outside')}</span>
            {#if round !== null}<span class="s">{t('node.role.nextRound', { time: clock(round) })}</span>{/if}
          {/if}
        </section>
        <section class="t kpi">
          <span class="l">{t('node.kpi.inPool')}</span>
          <span class="v">{days || '<1'} <small>{tp('unit.day', days || 2)}</small></span>
          <span class="s">{t('node.kpi.since', { date: formatWhen(Date.parse(n.registered_at)) })}</span>
        </section>
      </div>

      {#if n.state === 'quarantine'}<QuarantineNote reasons={n.quarantine_reasons?.length ? n.quarantine_reasons : n.failed} suspected={n.suspected} />{/if}

      {#if n.last_round}
        <section class="t">
          <div class="th">
            <h2>{t(`node.round.${n.last_round.verdict}`)}</h2>
            <span class="sub">{t(`node.round.${n.last_round.verdict}Text`)}</span>
            <span class="sp"></span>
            <span class="faint sm nowrap">
              {formatWhen(Date.parse(n.last_round.at))}{#if round !== null}, {t('node.round.next', { time: clock(round) })}{/if}
            </span>
          </div>
          <div class="tb-b"><Votes votes={n.last_round.votes} /></div>
        </section>
      {/if}

      <section class="t">
        <div class="th"><h2>{t('panel.node.about')}</h2></div>
        <dl class="tb-b about">
          <div><dt>{t('panel.node.owner')}</dt><dd>{n.owner_name ?? '—'}</dd></div>
          <div><dt>{t('panel.node.agent')}</dt><dd>{n.agent_version ?? '—'}</dd></div>
          <div><dt>{t('panel.node.heartbeat')}</dt><dd>{n.last_heartbeat ? formatWhen(Date.parse(n.last_heartbeat)) : t('panel.nodes.noSignal')}</dd></div>
          <div><dt>{t('panel.node.lastMeasure')}</dt><dd>{n.last_report_at ? formatWhen(Date.parse(n.last_report_at)) : t('panel.node.noMeasure')}</dd></div>
          <div><dt>{t('panel.node.failed')}</dt><dd>{n.failed.length ? whyText(n.failed) : t('panel.node.allGood')}</dd></div>
        </dl>
      </section>

      <section class="t">
        <div class="th">
          <h2>{t('panel.rounds.title')}</h2>
          <span class="sub">{t('panel.rounds.sub')}</span>
        </div>
        {#if n.rounds.length === 0}
          <div class="tb-b"><p class="muted">{t('node.round.none')}</p></div>
        {:else}
          <div class="tw">
            <table class="tb rounds">
              <thead>
                <tr>
                  <th>{t('panel.rounds.when')}</th>
                  <th>{t('panel.rounds.verdict')}</th>
                  {#each SENSORS as s (s)}<th>{t(`sensor.${s === 'red_flag' ? 'redflag' : s}` as 'sensor.globalping')}</th>{/each}
                </tr>
              </thead>
              <tbody>
                {#each n.rounds as round, i (i)}
                  <tr>
                    <td>{formatWhen(Date.parse(round.at))}</td>
                    <td>{t(`panel.verdict.${round.verdict}` as 'panel.verdict.clean')}</td>
                    {#each SENSORS as s (s)}
                      <td><VoteCell vote={round.votes.find((v) => v.sensor === s)} /></td>
                    {/each}
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
          <div class="tb-b"><p class="muted sm">{t('panel.node.hint')}</p></div>
        {/if}
      </section>
    {:else if !node.settled}
      <div class="kpis"><section class="t kpi"><div class="sk" style="height:60px"></div></section></div>
    {/if}

    <section class="t">
      <div class="th">
        <h2>{t('node.history.title')}</h2>
        <span class="sub">{t('node.history.sub')}</span>
      </div>
      <div class="tb-b">
        {#if slots}
          <HistStrip {slots} />
          <div class="row faint xs" style="justify-content:space-between;margin-top:6px">
            <span>{t('node.history.start')}</span><span>{t('node.history.end')}</span>
          </div>
        {:else}
          <p class="muted sm">{t('chart.noData')}</p>
        {/if}
      </div>
    </section>

    <section class="t">
      <div class="th">
        <h2>{t('panel.node.events')}</h2>
        <span class="sp"></span>
        <a href={hrefOf('journal')}>{t('panel.overview.allEvents')}</a>
      </div>
      {#each mine as e (e.id)}
        <EventRow event={e} now={clock1.ms} />
      {:else}
        <div class="tb-b"><p class="muted">{t('panel.journal.none')}</p></div>
      {/each}
    </section>
  {/if}
</div>

<RetireDialog bind:id={retiring} onretired={() => (window.location.hash = hrefOf('nodes'))} />

<style>
  .about {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(220px, 100%), 1fr));
    gap: 12px 20px;
    margin: 0;
  }
  .about > div {
    min-width: 0;
  }
  .about dt {
    color: var(--fg3);
    font-size: var(--fs-xs);
  }
  .about dd {
    margin: 2px 0 0;
    overflow-wrap: anywhere;
  }
</style>
