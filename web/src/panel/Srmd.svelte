<script lang="ts">
  import { onMount } from 'svelte';
  import { shortDomain } from '../lib/model';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import { t } from './i18n';
  import { useData } from './data.svelte';
  import { usePolled } from './live.svelte';
  import { FORMS } from './settings-fields';
  import { createSettings } from './settings-store.svelte';
  import SettingsForm from './SettingsForm.svelte';
  import type { SrmdOverview } from './types';
  import Confirm from './ui/Confirm.svelte';
  import Icon from './ui/Icon.svelte';

  // What the domain manager decided: how many domains the pool needs, which are folded onto another,
  // who manages each domain, and the manager's own settings.
  const panel = usePanel();
  const srmd = usePolled<SrmdOverview>(() => '/api/v1/admin/srmd');
  const pool = useData();
  const store = createSettings(panel);
  const queued = $derived((pool.nodes.data ?? []).filter((n) => n.state === 'healthy').length);
  let releasing = $state<string | null>(null);
  let busy = $state(false);

  onMount(() => {
    void store.load();
  });

  const form = FORMS.sr;
  const base = $derived(store.original['srmd.base_domain'] as string | undefined);
  const enabled = $derived(store.doc ? store.original['srmd.enabled'] === true : (srmd.data?.enabled ?? false));
  const maxPer = $derived(Number(store.original['srmd.max_nodes_per_domain']) || 0);

  const folded = $derived(srmd.data?.folded ?? []);
  const rows = $derived(
    srmd.data
      ? [
          ...srmd.data.active.map((domain) => ({ domain, target: null as string | null })),
          ...folded.map((domain) => ({ domain, target: srmd.data!.cnames[domain] ?? null })),
        ]
      : [],
  );
  // Who manages a domain: the main one, one the manager created, or one added by hand.
  const owner = (domain: string): 'main' | 'srmd' | 'manual' =>
    domain === base ? 'main' : srmd.data?.created.includes(domain) ? 'srmd' : 'manual';
  // Folded domains grouped by the one they point at, for the tree.
  const tree = $derived.by(() => {
    const map = new Map<string, string[]>((srmd.data?.active ?? []).map((d) => [d, []]));
    for (const d of folded) {
      const to = srmd.data?.cnames[d];
      if (to) map.set(to, [...(map.get(to) ?? []), d]);
    }
    return [...map.entries()];
  });

  const toggle = async (on: boolean) => {
    await store.setNow('srmd.enabled', on);
    srmd.refresh();
  };
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
    <span class="sp"></span>
    {#if srmd.data}
      <label class="sw">
        <input type="checkbox" data-key="srmd.enabled" checked={enabled} disabled={store.saving || !store.doc} onchange={(e) => toggle(e.currentTarget.checked)} />
        <span class="k"></span>
        <span class="sm">{enabled ? t('panel.srmd.on') : t('panel.srmd.off')}</span>
      </label>
    {/if}
    <span class="sub">{t('panel.srmd.sub')}</span>
  </header>
  {#if srmd.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}
  {#if store.doc && enabled && !base && !store.original['domains']}<p class="note" role="status">{t('panel.srmd.noBase')}</p>{/if}

  {#if !srmd.settled}
    <p class="muted">{t('common.loading')}</p>
  {:else if !srmd.data}
    <p class="muted">{t('error.network')}</p>
  {:else}
    {@const data = srmd.data}
    <div class="kpis">
      <section class="t kpi">
        <span class="l">{t('panel.srmd.needed')}</span>
        <span class="v">{data.needed}</span>
        <span class="s">{t('panel.srmd.neededNote', { n: queued, max: maxPer })}</span>
      </section>
      <section class="t kpi">
        <span class="l">{t('panel.srmd.active')}</span>
        <span class="v">{data.active.length}</span>
        <span class="s">{t('panel.srmd.activeNote')}</span>
      </section>
      <section class="t kpi">
        <span class="l">{t('panel.srmd.folded')}</span>
        <span class="v">{folded.length}</span>
        <span class="s">{folded.length > 0 ? t('panel.srmd.foldedNote') : t('panel.srmd.foldedNone')}</span>
      </section>
    </div>

    <section class="t">
      <div class="th">
        <h2>{t('panel.srmd.tree')}</h2>
        <span class="sub">{t('panel.srmd.treeSub')}</span>
      </div>
      {#if tree.length === 0}
        <div class="tb-b"><p class="muted">{t('panel.srmd.treeEmpty')}</p></div>
      {:else}
        <div class="tb-b dtree">
          {#each tree as [to, list] (to)}
            <div class="dt">
              <div class="card">
                <div class="d">{to}</div>
                <div class="s" style="margin-top:2px">{t('panel.srmd.treeNote', { n: data.clients[to] ?? 0 })}</div>
              </div>
              {#if list.length > 0}
                <div class="fds">
                  {#each list as d, j (d)}
                    <div class="fd">
                      <span class="ftk" style="--j:{j}"><i class="run fl" style="--dl:{(j * 0.9).toFixed(1)}s"></i></span>
                      <div class="card fc">
                        <div class="a"><span class="d">{d}</span><span class="tag">{t('panel.srmd.foldedTag')}</span></div>
                        <div class="s" style="margin-top:2px">{t('panel.srmd.foldedText', { short: shortDomain(to) })}</div>
                      </div>
                    </div>
                  {/each}
                </div>
              {/if}
            </div>
          {/each}
        </div>
      {/if}
    </section>

    <section class="t">
      <div class="th">
        <h2>{t('panel.srmd.domains')}</h2>
        <span class="sub">{t('panel.srmd.domainsSub')}</span>
      </div>
      <div class="tw">
        <table class="tb list">
          <thead>
            <tr>
              <th>{t('panel.masters.domain')}</th>
              <th>{t('panel.srmd.manages')}</th>
              <th>{t('stats.col.state')}</th>
              <th class="n">{t('stats.col.clients')}</th>
              <th><span class="sr-only">{t('panel.table.actions')}</span></th>
            </tr>
          </thead>
          <tbody>
            {#each rows as row (row.domain)}
              {@const who = owner(row.domain)}
              <tr>
                <td class="full mono nm" data-l={t('panel.masters.domain')}>{row.domain}</td>
                <td data-l={t('panel.srmd.manages')}>
                  {#if who === 'main'}<span class="tag acc">{t('panel.srmd.main')}</span>{:else if who === 'srmd'}{t('panel.srmd.bySrmd')}{:else}{t('panel.srmd.manual')}{/if}
                </td>
                <td data-l={t('stats.col.state')}>
                  {#if row.target}{t('panel.srmd.foldedInto', { domain: row.target })}{:else}{t('panel.srmd.working')}{/if}
                </td>
                <td class="n" data-l={t('stats.col.clients')}>{data.clients[row.domain] ?? '—'}</td>
                <td class="n">
                  {#if who === 'manual'}
                    <button class="btn s" type="button" onclick={() => take(row.domain)}>{t('panel.srmd.give')}</button>
                  {:else if who === 'srmd'}
                    <button class="btn s dng" type="button" onclick={() => (releasing = row.domain)}>{t('panel.srmd.takeManual')}</button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>

    {#if store.doc}
      <SettingsForm {store} {form} title={t('panel.srmd.settings')} sub={t('panel.srmd.settingsSub')} />
    {/if}
  {/if}
</div>

<Confirm
  bind:open={() => releasing !== null, (v) => { if (!v) releasing = null; }}
  title={t('panel.srmd.releaseTitle', { domain: releasing ?? '' })}
  text={t('panel.srmd.releaseText')}
  keep={t('panel.srmd.releaseKeep')}
  confirm={t('panel.srmd.releaseConfirm')}
  {busy}
  onconfirm={release}
/>
