<script lang="ts">
  import { useNow } from '../lib/clock.svelte';
  import { formatWhen } from '../lib/dates';
  import { t, tp } from '../lib/i18n';
  import { clock, ENGINE_NAMES, formatAvailability, historySlots, secondsUntil, shortDomain } from '../lib/model';
  import { withLiveClients } from '../lib/clients-live';
  import { useClientsLive } from '../lib/clients-live.svelte';
  import { useResource } from '../lib/resource.svelte';
  import type { NodeHistory, PublicNode } from '../lib/types';
  import HistStrip from '../shared/HistStrip.svelte';
  import Icon from '../shared/Icon.svelte';
  import MasterTag from '../shared/MasterTag.svelte';
  import QuarantineNote from '../shared/QuarantineNote.svelte';
  import NodeClientsKpi from '../shared/NodeClientsKpi.svelte';
  import StateCell from '../shared/StateCell.svelte';
  import Votes from '../shared/Votes.svelte';
  import PublicShell from './PublicShell.svelte';

  // One node: its state, the last round of checks with each sensor's vote, and the last day.
  let { id, fetchFn }: { id: string; fetchFn?: typeof fetch } = $props();

  const node = useResource<PublicNode>(() => `/api/v1/nodes/${encodeURIComponent(id)}`, fetchFn);
  const history = useResource<NodeHistory>(() => `/api/v1/nodes/${encodeURIComponent(id)}/history`, fetchFn);
  const clock1 = useNow(1000);
  const live = useClientsLive(() => {
    node.refresh();
    history.refresh();
  });

  const missing = $derived(node.data === null && node.status === 404);
  const slots = $derived(history.data ? historySlots(history.data, clock1.ms) : null);
  const spark = $derived(slots ? slots.map((s) => s.clients) : []);
  const days = $derived(node.data ? Math.max(0, Math.floor((clock1.ms - Date.parse(node.data.registered_at)) / 86_400_000)) : 0);
  const rotate = $derived(node.data ? secondsUntil(node.data.rotates_at, clock1.ms) : null);
  const round = $derived(node.data ? secondsUntil(node.data.next_round_at, clock1.ms) : null);
  const stale = $derived((node.stale || history.stale) && !missing);
</script>

<PublicShell active={null} updatedAt={missing ? clock1.ms : node.updatedAt} {stale}>
  {#if missing}
    <div class="t pad">
      <h1 class="b" style="font-size:16px">{t('node.missing.title')}</h1>
      <p class="muted">{t('node.missing.text')} <a href="/">{t('node.missing.link')}</a>.</p>
    </div>
  {:else}
    <div class="stack">
      <a class="row sm" href="/" style="width:max-content"><Icon name="back" />{t('node.back')}</a>
      {#if stale}<p class="stale">{t('common.stale')}</p>{/if}

      <div class="ph" style="margin:0">
        <h1>{id}</h1>
        {#if node.data}
          <StateCell state={node.data.state} suspected={node.data.suspected} />
          {#if node.data.is_master}<MasterTag domain={node.data.domains[0] ? shortDomain(node.data.domains[0]) : undefined} />{/if}
          <span class="sub"><span class="mono">{node.data.ip}:{node.data.port}</span>, {t('node.engine', { name: ENGINE_NAMES[node.data.engine] ?? node.data.engine })}</span>
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
            {:else if n.state === 'pending'}
              <span class="v">{t('node.role.notYet')}</span>
              <span class="s">{t('node.kpi.pendingText')}</span>
            {:else}
              <span class="v">{t('node.role.outside')}</span>
              {#if round !== null}<span class="s">{t('node.role.nextRound', { time: clock(round) })}</span>{/if}
            {/if}
          </section>
          <section class="t kpi">
            <span class="l">{t('node.kpi.inPool')}</span>
            <span class="v">{days || '<1'} <small>{days ? tp('unit.day', days) : tp('unit.day', 2)}</small></span>
            <span class="s">{t('node.kpi.since', { date: formatWhen(Date.parse(n.registered_at)) })}</span>
          </section>
        </div>

        {#if n.state === 'quarantine'}<QuarantineNote reasons={n.quarantine_reasons ?? []} suspected={n.suspected} />{/if}

        {#if n.state === 'healthy' && n.globalping_disputed}<p class="note" role="status">{t('check.disputed.globalping')}</p>{/if}

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
        {:else}
          <section class="t pad"><p class="muted">{t('node.round.none')}</p></section>
        {/if}
      {:else if !node.stale}
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
          {:else if !history.stale}
            <div class="sk" style="height:40px"></div>
          {/if}
        </div>
      </section>
    </div>
  {/if}
</PublicShell>
