<script lang="ts">
  import { useNow } from '../lib/clock.svelte';
  import { formatWhen } from '../lib/dates';
  import { t } from '../lib/i18n';
  import { ENGINE_NAMES, formatAvailability } from '../lib/model';
  import ChecksCell from '../shared/ChecksCell.svelte';
  import ClientsCell from '../shared/ClientsCell.svelte';
  import RoleCell from '../shared/RoleCell.svelte';
  import StateCell from '../shared/StateCell.svelte';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import { usePolled } from './live.svelte';
  import { failedText, panelGroups } from './nodes';
  import RetireDialog from './RetireDialog.svelte';
  import { hrefOf } from './route.svelte';
  import RowMenu from './RowMenu.svelte';
  import type { AdminNode } from './types';

  // Every node the pool counts on, in groups, and below them the ones it does not count on.
  const panel = usePanel();
  const nodes = usePolled<AdminNode[]>(() => '/api/v1/admin/nodes');
  const clock = useNow(1000);
  let retiring = $state<string | null>(null);

  const groups = $derived(nodes.data ? panelGroups(nodes.data) : null);
  const sections = $derived(
    groups
      ? [
          { key: 'masters', title: t('stats.group.masters'), rows: groups.masters },
          { key: 'queue', title: t('stats.group.queue'), rows: groups.queue },
          { key: 'quarantine', title: t('stats.group.quarantine'), rows: groups.quarantine },
        ].filter((s) => s.rows.length > 0)
      : [],
  );
  const maxClients = $derived(Math.max(1, ...sections.flatMap((s) => s.rows).map((n) => n.clients ?? 0)));

  const recheck = (id: string) =>
    act(panel, () => panel.client.post('/api/v1/admin/nodes/{id}/recheck', { params: { id } }), t('panel.nodes.rechecked', { id }));

  function open(event: MouseEvent): void {
    const row = event.currentTarget as HTMLElement;
    if ((event.target as Element).closest('a,button')) return;
    row.querySelector<HTMLAnchorElement>('a.nm')?.click();
  }
  const heartbeat = (n: AdminNode) => (n.last_heartbeat ? formatWhen(Date.parse(n.last_heartbeat)) : t('panel.nodes.noSignal'));
</script>

<div class="view">
  <header class="ph">
    <h1>{t('panel.nodes.title')}</h1>
  </header>

  {#if nodes.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}

  {#if !nodes.settled}
    <p class="muted">{t('common.loading')}</p>
  {:else if !groups}
    <p class="muted">{t('error.network')}</p>
  {:else}
    <div class="stack">
      <div class="t">
        <div class="tw">
          <table class="tb list">
            <thead>
              <tr>
                <th>{t('stats.col.node')}</th>
                <th>{t('stats.col.state')}</th>
                <th>{t('stats.col.role')}</th>
                <th>{t('stats.col.checks')}</th>
                <th class="n">{t('stats.col.availability')}</th>
                <th class="n">{t('stats.col.clients')}</th>
                <th><span class="sr-only">{t('panel.nodes.actions')}</span></th>
              </tr>
            </thead>
            <tbody>
              {#each sections as section (section.key)}
                <tr class="grp"><td colspan="7">{section.title}, {section.rows.length}</td></tr>
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
                    <td class="n">
                      <RowMenu id={node.id} onrecheck={() => recheck(node.id)} onretire={() => (retiring = node.id)} />
                    </td>
                  </tr>
                {/each}
              {/each}
            </tbody>
          </table>
        </div>
      </div>

      {#if groups.idle.length > 0}
        <section class="t">
          <div class="th"><h2>{t('panel.nodes.idle')}, {groups.idle.length}</h2></div>
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
                {#each groups.idle as node (node.id)}
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
