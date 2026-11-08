<script lang="ts">
  import { t } from '../lib/i18n';
  import { ENGINE_NAMES, formatAvailability, type NodeGroups } from '../lib/model';
  import ChecksCell from './ChecksCell.svelte';
  import ClientsCell from './ClientsCell.svelte';
  import RoleCell from './RoleCell.svelte';
  import StateCell from './StateCell.svelte';

  // The nodes of the pool in four groups. The table knows nothing of routes: `href` says where a
  // node's page is. Below 720px the stylesheet turns it into a list (`tb list`), and `data-l`
  // is the label each cell gets there.
  let {
    groups,
    href,
    now,
  }: { groups: NodeGroups; href: (id: string) => string; now: number } = $props();

  const sections = $derived([
    { key: 'masters', title: t('stats.group.masters'), nodes: groups.masters },
    { key: 'queue', title: t('stats.group.queue'), nodes: groups.queue },
    { key: 'quarantine', title: t('stats.group.quarantine'), nodes: groups.quarantine },
    { key: 'outside', title: t('stats.group.outside'), nodes: groups.outside },
  ].filter((s) => s.nodes.length > 0));

  const maxClients = $derived(
    Math.max(
      1,
      ...[...groups.masters, ...groups.queue, ...groups.quarantine, ...groups.outside].map((n) => n.clients ?? 0),
    ),
  );

  // A click anywhere on the row opens the node, like its link does; the link stays for keyboards.
  function open(event: MouseEvent): void {
    const row = event.currentTarget as HTMLElement;
    if ((event.target as Element).closest('a')) return;
    row.querySelector<HTMLAnchorElement>('a')?.click();
  }

</script>

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
      </tr>
    </thead>
    <tbody>
      {#each sections as section (section.key)}
        <tr class="grp"><td colspan="6">{section.title}, {section.nodes.length}</td></tr>
        {#each section.nodes as node (node.id)}
          <tr class={['go', node.is_master && 'mrow']} onclick={open}>
            <td class="full" data-l=""
              ><a class="nm" href={href(node.id)}>{node.id}</a>
              <div class="nm-s"><span class="mono">{node.ip}:{node.port}</span>, {ENGINE_NAMES[node.engine]}</div></td
            >
            <td data-l={t('stats.col.state')}><StateCell state={node.state} suspected={node.suspected} reasons={node.quarantine_reasons} /></td>
            <td data-l={t('stats.col.role')}><RoleCell {node} {now} /></td>
            <td data-l={t('stats.col.checks')}><ChecksCell checks={node.checks} disputed={node.globalping_disputed} /></td>
            <td class="n" data-l={t('stats.col.availability')}>{formatAvailability(node.availability_bp)}</td>
            <td class="n" data-l={t('stats.col.clients')}><ClientsCell clients={node.clients} max={maxClients} other={node.other_clients} /></td>
          </tr>
        {/each}
      {/each}
    </tbody>
  </table>
</div>
