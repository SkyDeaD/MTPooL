<script lang="ts">
  import { useNow } from '../lib/clock.svelte';
  import { formatNumber, formatWhen } from '../lib/dates';
  import { clock, formatAvailability, secondsUntil } from '../lib/model';
  import type { PublicDomain } from '../lib/types';
  import MasterTag from '../shared/MasterTag.svelte';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import { useData } from './data.svelte';
  import { t } from './i18n';
  import { usePolled } from './live.svelte';
  import { hrefOf } from './route.svelte';
  import Icon from './ui/Icon.svelte';

  // Who holds each domain right now, whether DNS matches, and who is next in the queue.
  const panel = usePanel();
  const data = useData();
  const domains = usePolled<PublicDomain[]>(() => '/api/v1/domains');
  const clock1 = useNow(1000);
  let pushing = $state(false);

  const nodes = $derived(data.nodes.data ?? []);
  const status = $derived(data.status.data);
  const byId = $derived(new Map(nodes.map((n) => [n.id, n])));
  const error = $derived(status?.dns.last_error ?? null);
  const queue = $derived(
    nodes
      .filter((n) => n.state === 'healthy' && !n.is_master)
      .sort((a, b) => (a.queue_position ?? 1e9) - (b.queue_position ?? 1e9))
      .slice(0, 8),
  );
  const stale = $derived(domains.stale || data.nodes.stale || data.status.stale);

  // How much of its term a master has served, for the bar under the countdown.
  function served(id: string): number {
    const n = byId.get(id);
    if (!n?.master_since || !n.rotates_at) return 0;
    const from = Date.parse(n.master_since);
    const to = Date.parse(n.rotates_at);
    return to > from ? Math.min(100, Math.max(0, ((clock1.ms - from) / (to - from)) * 100)) : 0;
  }
  const short = (d: string) => d.split('.')[0];

  async function push(): Promise<void> {
    pushing = true;
    await act(panel, () => panel.client.post('/api/v1/admin/dns/push', {}), t('panel.masters.pushed'));
    pushing = false;
  }
</script>

<div class="view stack">
  <header class="ph">
    <h1>{t('panel.masters.title')}</h1>
    <span class="sp"></span>
    <button class="btn" type="button" disabled={pushing} onclick={push}><Icon name="ref" />{t('panel.masters.push')}</button>
    <span class="sub">{t('panel.masters.sub')}</span>
  </header>
  {#if stale}<p class="note" role="status">{t('common.stale')}</p>{/if}

  {#if error}
    <div class="banner" role="alert">
      <span class="i"><Icon name="stop" /></span>
      <div class="tx">
        <b>{t('panel.cf.errorTitle')}</b>
        <span class="mono">{error}</span>
        <div class="muted">
          {t('panel.cf.errorText', {
            when: status?.dns.last_failed_at ? formatWhen(Date.parse(status.dns.last_failed_at)) : '—',
            count: status?.dns.unsynced_domains ?? 0,
          })}
        </div>
      </div>
    </div>
  {/if}

  <section class="t">
    <div class="th">
      <h2>{t('panel.masters.domains')}</h2>
      <span class="sub">{t('panel.masters.domainsSub')}</span>
    </div>
    <div class="tw">
      <table class="tb list">
        <thead>
          <tr>
            <th>{t('panel.masters.domain')}</th>
            <th>{t('panel.masters.dns')}</th>
            <th>{t('panel.masters.holder')}</th>
            <th>{t('panel.masters.rotation')}</th>
            <th class="n">{t('stats.col.clients')}</th>
            <th>{t('panel.masters.dnsState')}</th>
          </tr>
        </thead>
        <tbody>
          {#each domains.data ?? [] as d (d.domain)}
            {#if d.folded_into}
              <tr class="foldr">
                <td class="full mono nm" data-l={t('panel.masters.domain')}><span class="row"><Icon name="elbow" cls="elb" />{d.domain}</span></td>
                <td class="mono sm" data-l={t('panel.masters.dns')}>CNAME {d.dns?.values[0] ?? d.folded_into}</td>
                <td class="faint" data-l={t('panel.masters.holder')}>{t('panel.masters.foldedNoMasters')}</td>
                <td class="faint hm" data-l={t('panel.masters.rotation')}>{t('panel.masters.none2')}</td>
                <td class="n faint hm" data-l={t('stats.col.clients')}>{t('panel.masters.at', { name: short(d.folded_into) })}</td>
                <td data-l={t('panel.masters.dnsState')}>
                  <span class={['st', d.dns_synced ? 'ok' : 'bad']}><Icon name={d.dns_synced ? 'check' : 'alert'} />{d.dns_synced ? t('panel.masters.synced') : t('panel.masters.unsynced')}</span>
                </td>
              </tr>
            {:else if d.masters.length === 0}
              <tr>
                <td class="full mono nm" data-l={t('panel.masters.domain')}>{d.domain}</td>
                <td class="mono sm" data-l={t('panel.masters.dns')}>{d.dns ? `${d.dns.type} ${d.dns.values.join(', ')}` : '—'}</td>
                <td class="muted" data-l={t('panel.masters.holder')}>{t('panel.masters.none')}</td>
                <td class="faint hm" data-l={t('panel.masters.rotation')}>—</td>
                <td class="n" data-l={t('stats.col.clients')}>{d.clients !== null && d.clients !== undefined ? formatNumber(d.clients) : '—'}</td>
                <td data-l={t('panel.masters.dnsState')}>
                  <span class={['st', d.dns_synced ? 'ok' : 'bad']}><Icon name={d.dns_synced ? 'check' : 'alert'} />{d.dns_synced ? t('panel.masters.synced') : t('panel.masters.unsynced')}</span>
                </td>
              </tr>
            {:else}
              {#each d.masters as id, i (id)}
                {@const n = byId.get(id)}
                {@const left = n ? secondsUntil(n.rotates_at, clock1.ms) : null}
                <tr>
                  {#if i === 0}
                    <td class="full mono nm tp" rowspan={d.masters.length} data-l={t('panel.masters.domain')}>{d.domain}</td>
                    <td class="mono sm tp" rowspan={d.masters.length} data-l={t('panel.masters.dns')}>
                      {d.dns ? `${d.dns.type} ${d.dns.values.join(', ')}` : '—'}
                    </td>
                  {/if}
                  <td data-l={t('panel.masters.holder')}><MasterTag /> <a class="nm" href={hrefOf('node', id)}>{id}</a></td>
                  <td data-l={t('panel.masters.rotation')}>
                    {#if left !== null}
                      <span class="sm">{clock(left)}</span>
                      <span class="bar"><i style="width:{served(id)}%"></i></span>
                    {:else}
                      <span class="faint">—</span>
                    {/if}
                  </td>
                  <td class="n" data-l={t('stats.col.clients')}>{n?.clients ?? '—'}</td>
                  {#if i === 0}
                    <td class="tp" rowspan={d.masters.length} data-l={t('panel.masters.dnsState')}>
                      <span class={['st', d.dns_synced ? 'ok' : 'bad']}><Icon name={d.dns_synced ? 'check' : 'alert'} />{d.dns_synced ? t('panel.masters.synced') : t('panel.masters.unsynced')}</span>
                    </td>
                  {/if}
                </tr>
              {/each}
            {/if}
          {/each}
        </tbody>
      </table>
    </div>
  </section>

  <div class="cols c84">
    <section class="t">
      <div class="th">
        <h2>{t('panel.masters.queue')}</h2>
        <span class="sub">{t('panel.masters.queueSub')}</span>
      </div>
      {#if queue.length === 0}
        <div class="tb-b"><p class="muted">{t('panel.masters.queueEmpty')}</p></div>
      {:else}
        <div class="tw">
          <table class="tb list">
            <thead>
              <tr>
                <th>{t('panel.masters.place')}</th>
                <th>{t('stats.col.node')}</th>
                <th class="n">{t('stats.col.availability')}</th>
                <th class="n">{t('stats.col.clients')}</th>
              </tr>
            </thead>
            <tbody>
              {#each queue as n, i (n.id)}
                <tr>
                  <td data-l={t('panel.masters.place')}>
                    {n.queue_position ?? i + 1}{#if i === 0} <span class="tag acc">{t('panel.masters.next')}</span>{/if}
                  </td>
                  <td class="full" data-l={t('stats.col.node')}><a class="nm" href={hrefOf('node', n.id)}>{n.id}</a></td>
                  <td class="n" data-l={t('stats.col.availability')}>{formatAvailability(n.availability_bp)}</td>
                  <td class="n" data-l={t('stats.col.clients')}>{n.clients ?? '—'}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </section>

    <section class="t">
      <div class="th"><h2>{t('panel.masters.cloudflare')}</h2></div>
      <dl class="tb-b about">
        <dt>{t('panel.overview.dnsToken')}</dt>
        <dd>{status ? (status.dns.token_set ? t('panel.overview.dnsTokenSet') : t('panel.overview.dnsTokenUnset')) : '—'}</dd>
        <dt>{t('panel.overview.dnsLast')}</dt>
        <dd>{status?.dns.last_applied_at ? formatWhen(Date.parse(status.dns.last_applied_at)) : '—'}</dd>
        <dt>{t('panel.overview.dnsApplied')}</dt>
        <dd>{status?.dns.applied_24h ?? '—'}</dd>
      </dl>
      <div class="tb-b"><a href={hrefOf('settings')}>{t('panel.masters.dnsSettings')}</a></div>
    </section>
  </div>
</div>
