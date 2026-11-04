<script lang="ts">
  import { useNow } from '../lib/clock.svelte';
  import { formatWhen } from '../lib/dates';
  import { t } from './i18n';
  import { ENGINE_NAMES, formatAvailability } from '../lib/model';
  import ChecksCell from '../shared/ChecksCell.svelte';
  import ClientsCell from '../shared/ClientsCell.svelte';
  import RoleCell from '../shared/RoleCell.svelte';
  import StateCell from '../shared/StateCell.svelte';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import AddNode from './AddNode.svelte';
  import { useData } from './data.svelte';
  import Icon from './ui/Icon.svelte';
  import { failedText, panelGroups } from './nodes';
  import RetireDialog from './RetireDialog.svelte';
  import { hrefOf } from './route.svelte';
  import RowMenu from './RowMenu.svelte';
  import type { AdminNode } from './types';

  // Every node the pool counts on, in groups, and below them the ones it does not count on.
  const panel = usePanel();
  const nodes = useData().nodes;
  let adding = $state(false);
  let query = $state('');
  let filter = $state<'all' | 'master' | 'queue' | 'prob'>('all');
  let sort = $state<'availability' | 'clients' | null>(null);
  const clock = useNow(1000);
  let retiring = $state<string | null>(null);

  const groups = $derived(nodes.data ? panelGroups(nodes.data) : null);
  const match = (n: AdminNode) => {
    const q = query.trim().toLowerCase();
    return !q || `${n.id} ${n.full_ip} ${ENGINE_NAMES[n.engine]}`.toLowerCase().includes(q);
  };
  const count = $derived({
    all: groups ? groups.masters.length + groups.queue.length + groups.quarantine.length : 0,
    master: groups?.masters.length ?? 0,
    queue: groups?.queue.length ?? 0,
    prob: groups?.quarantine.length ?? 0,
  });
  const order = (rows: AdminNode[]) => {
    if (!sort) return rows;
    const key = (n: AdminNode) => (sort === 'availability' ? n.availability_bp : n.clients) ?? -1;
    return [...rows].sort((a, b) => key(b) - key(a));
  };
  const sections = $derived(
    groups
      ? [
          { key: 'prob', title: t('panel.nodes.group.quarantine'), rows: groups.quarantine },
          { key: 'master', title: t('panel.nodes.group.masters'), rows: groups.masters },
          { key: 'queue', title: t('panel.nodes.group.queue'), rows: groups.queue },
        ]
          .filter((s) => filter === 'all' || filter === s.key)
          .map((s) => ({ ...s, rows: order(s.rows.filter(match)) }))
          .filter((s) => s.rows.length > 0)
      : [],
  );
  const idle = $derived(groups ? groups.idle.filter(match) : []);
  const maxClients = $derived(Math.max(1, ...sections.flatMap((s) => s.rows).map((n) => n.clients ?? 0)));

  const recheck = (id: string) =>
    act(panel, () => panel.client.post('/api/v1/admin/nodes/{id}/recheck', { params: { id } }), t('panel.nodes.rechecked', { id }));

  function open(event: MouseEvent): void {
    const row = event.currentTarget as HTMLElement;
    if ((event.target as Element).closest('a,button')) return;
    row.querySelector<HTMLAnchorElement>('a.nm')?.click();
  }
  const heartbeat = (n: AdminNode) => {
    if (!n.last_heartbeat) return t('panel.nodes.noSignal');
    const at = Date.parse(n.last_heartbeat);
    const s = Math.max(0, Math.round((clock.ms - at) / 1000));
    if (s < 120) return t('panel.nodes.ago', { s });
    return s < 3600 ? t('panel.nodes.agoMin', { m: Math.round(s / 60) }) : formatWhen(at);
  };
</script>

<div class="view">
  <header class="ph">
    <h1>{t('panel.nodes.title')}</h1>
    <span class="sp"></span>
    <button class="btn pri" type="button" onclick={() => (adding = true)}><Icon name="plus" />{t('panel.nodes.add')}</button>
  </header>

  {#if nodes.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}

  {#if !nodes.settled}
    <p class="muted">{t('common.loading')}</p>
  {:else if !groups}
    <p class="muted">{t('error.network')}</p>
  {:else}
    <div class="stack">
      <div class="t">
        <div class="th">
          <div class="srch">
            <Icon name="search" />
            <input class="inp" type="search" bind:value={query} placeholder={t('panel.nodes.search')} aria-label={t('panel.nodes.searchLabel')} />
          </div>
          <div class="chips">
            {#each [['all', 'panel.nodes.filter.all'], ['master', 'panel.nodes.filter.master'], ['queue', 'panel.nodes.filter.queue'], ['prob', 'panel.nodes.filter.prob']] as const as [k, label] (k)}
              <button class={['chip', k === 'prob' && count.prob > 0 && 'bad']} type="button" aria-pressed={filter === k} onclick={() => (filter = k)}>
                {t(label)} <b>{count[k]}</b>
              </button>
            {/each}
          </div>
        </div>
        <div class="tw">
          <table class="tb list">
            <thead>
              <tr>
                <th>{t('stats.col.node')}</th>
                <th>{t('stats.col.state')}</th>
                <th>{t('stats.col.role')}</th>
                <th>{t('stats.col.checks')}</th>
                <th class="n"><button type="button" onclick={() => (sort = 'availability')}>{t('stats.col.availability')}{sort === 'availability' ? ' ↓' : ''}</button></th>
                <th class="n"><button type="button" onclick={() => (sort = 'clients')}>{t('stats.col.clients')}{sort === 'clients' ? ' ↓' : ''}</button></th>
                <th>{t('panel.nodes.signal')}</th>
                <th><span class="sr-only">{t('panel.nodes.actions')}</span></th>
              </tr>
            </thead>
            <tbody>
              {#each sections as section (section.key)}
                <tr class="grp"><td colspan="8">{section.title}, {section.rows.length}</td></tr>
                {#each section.rows as node (node.id)}
                  <tr class={['go', node.is_master && 'mrow']} onclick={open}>
                    <td class="full" data-l=""
                      ><a class="nm" href={hrefOf('node', node.id)}>{node.id}</a>
                      <div class="nm-s">
                        <span class="mono">{node.full_ip}:{node.port}</span>, {ENGINE_NAMES[node.engine]}{#if node.owner_name}, {node.owner_name}{/if}
                      </div></td
                    >
                    <td data-l={t('stats.col.state')}><StateCell state={node.state} /></td>
                    <td data-l={t('stats.col.role')}><RoleCell {node} now={clock.ms} /></td>
                    <td data-l={t('stats.col.checks')}><ChecksCell checks={node.checks} /></td>
                    <td class="n" data-l={t('stats.col.availability')}>{formatAvailability(node.availability_bp)}</td>
                    <td class="n" data-l={t('stats.col.clients')}><ClientsCell clients={node.clients} max={maxClients} /></td>
                    <td class="nowrap" data-l={t('panel.nodes.signal')}>{heartbeat(node)}</td>
                    <td class="n">
                      <RowMenu id={node.id} onrecheck={() => recheck(node.id)} onretire={() => (retiring = node.id)} />
                    </td>
                  </tr>
                {/each}
              {:else}
                <tr><td colspan="8" class="muted" style="text-align:center;padding:28px">{t('panel.nodes.none')}</td></tr>
              {/each}
            </tbody>
          </table>
        </div>
      </div>

      {#if idle.length > 0}
        <section class="t">
          <div class="th"><h2>{t('panel.nodes.idle')}, {idle.length}</h2></div>
          <div class="tw">
            <table class="tb list">
              <thead>
                <tr>
                  <th>{t('stats.col.node')}</th>
                  <th>{t('stats.col.state')}</th>
                  <th>{t('panel.nodes.reason')}</th>
                  <th>{t('panel.nodes.lastSignal')}</th>
                  <th><span class="sr-only">{t('panel.nodes.actions')}</span></th>
                </tr>
              </thead>
              <tbody>
                {#each idle as node (node.id)}
                  <tr class="go" onclick={open}>
                    <td class="full" data-l=""
                      ><a class="nm" href={hrefOf('node', node.id)}>{node.id}</a>
                      <div class="nm-s"><span class="mono">{node.full_ip}:{node.port}</span></div></td
                    >
                    <td data-l={t('stats.col.state')}>{t(`panel.state.${node.state}` as 'panel.state.pending')}</td>
                    <td data-l={t('panel.nodes.reason')}>{node.failed.length ? failedText(node.failed) : '—'}</td>
                    <td data-l={t('panel.nodes.lastSignal')}>{heartbeat(node)}</td>
                    <td class="n">
                      {#if node.state !== 'pruned'}
                        <RowMenu id={node.id} onrecheck={() => recheck(node.id)} onretire={() => (retiring = node.id)} />
                      {/if}
                    </td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        </section>
      {/if}
    </div>
  {/if}
</div>

<RetireDialog bind:id={retiring} />
<AddNode bind:open={adding} />
