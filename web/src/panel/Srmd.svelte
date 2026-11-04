<script lang="ts">
  import { t } from '../lib/i18n';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import { usePolled } from './live.svelte';
  import type { SrmdOverview } from './types';
  import Confirm from './ui/Confirm.svelte';

  // What the domain manager decided: which domains work, which are folded onto another, and
  // how many the queue needs now. A domain can be taken under its control or let go by hand.
  const panel = usePanel();
  const srmd = usePolled<SrmdOverview>(() => '/api/v1/admin/srmd');
  let releasing = $state<string | null>(null);
  let busy = $state(false);


  const rows = $derived(
    srmd.data
      ? [
          ...srmd.data.active.map((domain) => ({ domain, target: null as string | null })),
          ...srmd.data.folded.map((domain) => ({ domain, target: srmd.data!.cnames[domain] ?? null })),
        ]
      : [],
  );

  const take = (domain: string) =>
    act(panel, () => panel.client.post('/api/v1/admin/srmd/{domain}/take', { params: { domain }, body: {} }), t('panel.srmd.taken', { domain }));

  async function release(): Promise<void> {
    if (releasing === null) return;
    const domain = releasing;
    busy = true;
    await act(panel, () => panel.client.post('/api/v1/admin/srmd/{domain}/release', { params: { domain }, body: {} }), t('panel.srmd.released', { domain }));
    busy = false;
    releasing = null;
  }
</script>

<div class="view stack">
  <header class="ph">
    <h1>{t('panel.srmd.title')}</h1>
    {#if srmd.data}
      <span class="st {srmd.data.enabled ? 'ok' : 'off'}">{srmd.data.enabled ? t('panel.srmd.on') : t('panel.srmd.off')}</span>
    {/if}
  </header>
  {#if srmd.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}

  {#if !srmd.settled}
    <p class="muted">{t('common.loading')}</p>
  {:else if !srmd.data}
    <p class="muted">{t('error.network')}</p>
  {:else}
    <div class="kpis">
      <section class="t kpi">
        <span class="l">{t('panel.srmd.needed')}</span>
        <span class="v">{srmd.data.needed}</span>
        <span class="s">{t('panel.srmd.neededNote')}</span>
      </section>
      <section class="t kpi">
        <span class="l">{t('panel.srmd.active')}</span>
        <span class="v">{srmd.data.active.length}</span>
      </section>
      <section class="t kpi">
        <span class="l">{t('panel.srmd.folded')}</span>
        <span class="v">{srmd.data.folded.length}</span>
      </section>
      <section class="t kpi">
        <span class="l">{t('panel.srmd.created')}</span>
        <span class="v">{srmd.data.created.length}</span>
        <span class="s">{t('panel.srmd.createdNote')}</span>
      </section>
    </div>

    <div class="t">
      <div class="tw">
        <table class="tb list">
          <thead>
            <tr>
              <th>{t('panel.masters.domain')}</th>
              <th>{t('stats.col.state')}</th>
              <th class="n">{t('stats.col.clients')}</th>
              <th><span class="sr-only">{t('panel.nodes.actions')}</span></th>
            </tr>
          </thead>
          <tbody>
            {#each rows as row (row.domain)}
              <tr>
                <td class="full" data-l="">
                  <span class="mono">{row.domain}</span>
                  {#if srmd.data.created.includes(row.domain)}<span class="muted"> {t('panel.srmd.createdTag')}</span>{/if}
                </td>
                <td data-l={t('stats.col.state')}>
                  {#if row.target}{t('panel.srmd.foldedInto', { domain: row.target })}{:else}{t('panel.srmd.working')}{/if}
                </td>
                <td class="n" data-l={t('stats.col.clients')}>{srmd.data.clients[row.domain] ?? '—'}</td>
                <td class="n">
                  <div class="row-actions">
                    <button class="btn s" type="button" onclick={() => take(row.domain)}>{t('panel.srmd.take')}</button>
                    <button class="btn s dng" type="button" onclick={() => (releasing = row.domain)}>{t('panel.srmd.release')}</button>
                  </div>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </div>
  {/if}
</div>

<Confirm
  bind:open={() => releasing !== null, (v) => { if (!v) releasing = null; }}  title={t('panel.srmd.releaseTitle', { domain: releasing ?? '' })}
  text={t('panel.srmd.releaseText')}
  keep={t('panel.srmd.releaseKeep')}
  confirm={t('panel.srmd.releaseConfirm')}
  {busy}
  onconfirm={release}
/>
