<script lang="ts">
  import { useNow } from '../lib/clock.svelte';
  import { formatWhen } from '../lib/dates';
  import { t } from '../lib/i18n';
  import { formatAvailability } from '../lib/model';
  import type { PublicDomain } from '../lib/types';
  import StateCell from '../shared/StateCell.svelte';
  import { usePolled } from './live.svelte';
  import { failedText, panelGroups } from './nodes';
  import { hrefOf } from './route.svelte';
  import type { AdminNode, AdminStatus } from './types';
  import { sensorKey } from './types';

  // The pool at a glance: how many nodes work, what needs a look, and whether the checks and DNS are fine.
  const nodes = usePolled<AdminNode[]>(() => '/api/v1/admin/nodes');
  const status = usePolled<AdminStatus>(() => '/api/v1/admin/status');
  const domains = usePolled<PublicDomain[]>(() => '/api/v1/domains');
  useNow(1000);

  const groups = $derived(nodes.data ? panelGroups(nodes.data) : null);
  const working = $derived(groups ? groups.masters.length + groups.queue.length : null);
  const known = $derived(
    groups
      ? [...groups.masters, ...groups.queue, ...groups.quarantine]
          .map((n) => n.availability_bp)
          .filter((v): v is number => typeof v === 'number')
      : [],
  );
  const availability = $derived(known.length > 0 ? known.reduce((a, b) => a + b, 0) / known.length : null);
  const clients = $derived(
    domains.data ? domains.data.reduce((sum, d) => sum + (d.folded_into ? 0 : (d.clients ?? 0)), 0) : null,
  );
  // What needs a person: nodes in quarantine, a sensor that is not set up, a DNS that fails.
  const dnsBroken = $derived(!!status.data?.dns.last_error);
  const silent = $derived(status.data ? status.data.sensors.filter((s) => !s.configured) : []);
  const stale = $derived(nodes.stale || status.stale || domains.stale);
</script>

<div class="view stack">
  <header class="ph">
    <h1>{t('panel.overview.title')}</h1>
  </header>
  {#if stale}<p class="note" role="status">{t('common.stale')}</p>{/if}

  <div class="kpis">
    <section class="t kpi">
      <span class="l">{t('panel.overview.working')}</span>
      <span class="v">{working ?? '—'}</span>
      <span class="s">{t('panel.overview.workingNote', { masters: groups?.masters.length ?? 0 })}</span>
    </section>
    <section class="t kpi">
      <span class="l">{t('panel.overview.clients')}</span>
      <span class="v">{clients ?? '—'}</span>
      <span class="s">{t('panel.overview.clientsNote')}</span>
    </section>
    <section class="t kpi">
      <span class="l">{t('panel.overview.availability')}</span>
      <span class="v">{formatAvailability(availability)}</span>
      <span class="s">{t('panel.overview.availabilityNote')}</span>
    </section>
    <section class="t kpi">
      <span class="l">{t('panel.overview.changes')}</span>
      <span class="v">{status.data?.counts.master_changes_24h ?? '—'}</span>
      <span class="s">{t('panel.overview.changesNote')}</span>
    </section>
  </div>

  <section class="t">
    <div class="th"><h2>{t('panel.overview.attention')}</h2></div>
    {#if groups && !dnsBroken && silent.length === 0 && groups.quarantine.length === 0}
      <div class="tb-b calm">{t('panel.overview.calm')}</div>
    {:else}
      <ul class="tb-b attn">
        {#if dnsBroken}
          <li><a href={hrefOf('masters')}>{t('panel.overview.dnsFails')}</a></li>
        {/if}
        {#each silent as sensor (sensor.name)}
          <li><a href={hrefOf('settings')}>{t('panel.overview.sensorOff', { name: t(`sensor.${sensorKey(sensor.name)}` as 'sensor.globalping') })}</a></li>
        {/each}
        {#each groups?.quarantine ?? [] as node (node.id)}
          <li>
            <a class="nm" href={hrefOf('node', node.id)}>{node.id}</a>
            <StateCell state={node.state} />
            <span class="muted">{node.failed.length ? failedText(node.failed) : ''}</span>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <div class="cols">
    <section class="t">
      <div class="th"><h2>{t('panel.overview.sensors')}</h2></div>
      <div class="tw">
        <table class="tb">
          <thead>
            <tr>
              <th>{t('panel.overview.sensor')}</th>
              <th>{t('panel.overview.sensorState')}</th>
              <th>{t('panel.overview.lastAnswer')}</th>
            </tr>
          </thead>
          <tbody>
            {#each status.data?.sensors ?? [] as sensor (sensor.name)}
              <tr>
                <td>{t(`sensor.${sensorKey(sensor.name)}` as 'sensor.globalping')}</td>
                <td>{sensor.configured ? t('panel.overview.on') : t('panel.overview.off')}</td>
                <td>{sensor.last_answer_at ? formatWhen(Date.parse(sensor.last_answer_at)) : '—'}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>

    <section class="t">
      <div class="th"><h2>{t('panel.overview.dns')}</h2></div>
      <dl class="tb-b about">
        <div><dt>{t('panel.overview.dnsApplied')}</dt><dd>{status.data?.dns.applied_24h ?? '—'}</dd></div>
        <div><dt>{t('panel.overview.dnsFailed')}</dt><dd>{status.data?.dns.failed_24h ?? '—'}</dd></div>
        <div>
          <dt>{t('panel.overview.dnsLast')}</dt>
          <dd>{status.data?.dns.last_applied_at ? formatWhen(Date.parse(status.data.dns.last_applied_at)) : '—'}</dd>
        </div>
        <div>
          <dt>{t('panel.overview.dnsToken')}</dt>
          <dd>{status.data ? (status.data.dns.token_set ? t('panel.overview.dnsTokenSet') : t('panel.overview.dnsTokenUnset')) : '—'}</dd>
        </div>
      </dl>
    </section>
  </div>
</div>

<style>
  .about {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(160px, 100%), 1fr));
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
  }
</style>
