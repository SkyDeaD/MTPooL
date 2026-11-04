<script lang="ts">
  import { formatWhen } from '../lib/dates';
  import { t } from '../lib/i18n';
  import type { MessageKey } from '../strings.ru';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import { usePolled } from './live.svelte';
  import { hrefOf } from './route.svelte';
  import type { RolloutView } from './types';
  import Confirm from './ui/Confirm.svelte';

  // Releases going out to the nodes: the canary first, a watch, then batches. One can be
  // paused (continue it or call it off) or running (call it off).
  const panel = usePanel();
  const rollouts = usePolled<RolloutView[]>(() => '/api/v1/admin/rollouts');

  const STEPS = ['canary', 'observing', 'batches', 'done'] as const;
  const list = $derived(rollouts.data ?? []);
  const current = $derived(list.find((r) => r.phase !== 'done' && r.phase !== 'cancelled') ?? list[0] ?? null);
  const earlier = $derived(list.filter((r) => r !== current));

  /** Where the stepper stands: the index of the live step, with `paused` and `cancelled` marked on it. */
  function position(r: RolloutView): number {
    const phase = r.phase === 'paused' ? (r.resume_to ?? 'canary') : r.phase;
    const at = STEPS.indexOf(phase as (typeof STEPS)[number]);
    return at < 0 ? (r.phase === 'cancelled' ? 0 : STEPS.length - 1) : at;
  }
  const stepClass = (r: RolloutView, i: number) => {
    if (r.phase === 'done') return 'done';
    if (r.phase === 'cancelled') return i < position(r) ? 'done' : i === position(r) ? 'stop' : 'todo';
    if (r.phase === 'paused' && i === position(r)) return 'stop';
    return i < position(r) ? 'done' : i === position(r) ? 'now' : 'todo';
  };

  let cancelling = $state<RolloutView | null>(null);
  let busy = $state(false);

  const resume = (r: RolloutView) =>
    act(panel, () => panel.client.post('/api/v1/admin/rollouts/{id}/resume', { params: { id: r.id }, body: {} }), t('panel.updates.resumed'));
  async function cancel(): Promise<void> {
    if (!cancelling) return;
    const id = cancelling.id;
    busy = true;
    await act(panel, () => panel.client.post('/api/v1/admin/rollouts/{id}/cancel', { params: { id }, body: {} }), t('panel.updates.cancelled'));
    busy = false;
    cancelling = null;
  }
  const count = (r: RolloutView, key: keyof RolloutView['counts']) => r.counts[key];
</script>

<div class="view stack">
  <header class="ph"><h1>{t('panel.updates.title')}</h1></header>
  {#if rollouts.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}

  {#if !rollouts.settled}
    <p class="muted">{t('common.loading')}</p>
  {:else if !current}
    <p class="muted">{t('panel.updates.none')}</p>
  {:else}
    <section class="t">
      <div class="th">
        <h2>{t('panel.updates.release', { version: current.version })}</h2>
        <span class="sub">{t(`panel.updates.phase.${current.phase}` as MessageKey)}</span>
        <span class="sp"></span>
        <div class="row-actions">
          {#if current.can_resume}<button class="btn pri s" type="button" onclick={() => resume(current)}>{t('panel.updates.resume')}</button>{/if}
          {#if current.can_cancel}<button class="btn dng s" type="button" onclick={() => (cancelling = current)}>{t('panel.updates.cancel')}</button>{/if}
        </div>
      </div>

      <div class="stepper" role="list">
        {#each STEPS as step, i (step)}
          <div class={['sx', stepClass(current, i)]} role="listitem">
            <span class="ln"><i></i></span>
            <span class="a">{t(`panel.updates.step.${step}` as MessageKey)}</span>
            <span class="s">
              {#if step === 'canary' && current.canary}<span class="mono">{current.canary}</span>
              {:else if step === 'batches' && current.next_batch_at && current.phase === 'batches'}{t('panel.updates.nextBatch', { when: formatWhen(Date.parse(current.next_batch_at)) })}{/if}
            </span>
          </div>
        {/each}
      </div>

      {#if current.note}
        <div class="tb-b"><p class="note">{current.note.text}</p></div>
      {/if}

      <div class="kpis">
        {#each ['updated', 'offered', 'started', 'idle', 'failed', 'missed'] as const as key (key)}
          <section class="t kpi">
            <span class="l">{t(`panel.updates.count.${key}` as MessageKey)}</span>
            <span class="v">{count(current, key)}</span>
          </section>
        {/each}
      </div>

      {#if current.nodes.length > 0}
        <div class="tw">
          <table class="tb list">
            <thead>
              <tr>
                <th>{t('stats.col.node')}</th>
                <th>{t('panel.updates.stage')}</th>
                <th>{t('panel.updates.version')}</th>
                <th>{t('panel.updates.at')}</th>
              </tr>
            </thead>
            <tbody>
              {#each current.nodes as n (n.node_id)}
                <tr>
                  <td class="full" data-l=""><a class="nm" href={hrefOf('node', n.node_id)}>{n.node_id}</a></td>
                  <td data-l={t('panel.updates.stage')}>{t(`panel.updates.nodeStage.${n.stage}` as MessageKey)}</td>
                  <td data-l={t('panel.updates.version')}><span class="mono">{n.version ?? '—'}</span></td>
                  <td data-l={t('panel.updates.at')}>{n.at ? formatWhen(Date.parse(n.at)) : '—'}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </section>

    {#if earlier.length > 0}
      <section class="t">
        <div class="th"><h2>{t('panel.updates.earlier')}</h2></div>
        <div class="tw">
          <table class="tb list">
            <thead>
              <tr>
                <th>{t('panel.updates.version')}</th>
                <th>{t('stats.col.state')}</th>
                <th>{t('panel.updates.at')}</th>
              </tr>
            </thead>
            <tbody>
              {#each earlier as r (r.id)}
                <tr>
                  <td class="full" data-l=""><span class="mono">{r.version}</span></td>
                  <td data-l={t('stats.col.state')}>{t(`panel.updates.phase.${r.phase}` as MessageKey)}</td>
                  <td data-l={t('panel.updates.at')}>{formatWhen(Date.parse(r.changed_at ?? r.created_at))}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      </section>
    {/if}
  {/if}
</div>

<Confirm
  bind:open={() => cancelling !== null, (v) => { if (!v) cancelling = null; }}  title={t('panel.updates.cancelTitle', { version: cancelling?.version ?? '' })}
  text={t('panel.updates.cancelText')}
  keep={t('panel.updates.cancelKeep')}
  confirm={t('panel.updates.cancelConfirm')}
  {busy}
  onconfirm={cancel}
/>
