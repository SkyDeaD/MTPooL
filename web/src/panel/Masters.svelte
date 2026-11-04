<script lang="ts">
  import { useNow } from '../lib/clock.svelte';
  import { formatWhen } from '../lib/dates';
  import { t } from '../lib/i18n';
  import { clock, secondsUntil, shortDomain } from '../lib/model';
  import type { PublicDomain } from '../lib/types';
  import Icon from '../shared/Icon.svelte';
  import MasterTag from '../shared/MasterTag.svelte';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import { usePolled } from './live.svelte';
  import { hrefOf } from './route.svelte';
  import type { AdminNode, AdminStatus } from './types';

  // Who holds each domain right now, and whether Cloudflare took the last change.
  const panel = usePanel();
  const domains = usePolled<PublicDomain[]>(() => '/api/v1/domains');
  const nodes = usePolled<AdminNode[]>(() => '/api/v1/admin/nodes');
  const status = usePolled<AdminStatus>(() => '/api/v1/admin/status');
  const clock1 = useNow(1000);
  let pushing = $state(false);

  const byId = $derived(new Map((nodes.data ?? []).map((n) => [n.id, n])));
  const error = $derived(status.data?.dns.last_error ?? null);
  const stale = $derived(domains.stale || nodes.stale || status.stale);

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
            when: status.data?.dns.last_failed_at ? formatWhen(Date.parse(status.data.dns.last_failed_at)) : '—',
            count: status.data?.dns.unsynced_domains ?? 0,
          })}
        </div>
      </div>
    </div>
  {/if}

  <div class="t">
    <div class="tw">
      <table class="tb list">
        <thead>
          <tr>
            <th>{t('panel.masters.domain')}</th>
            <th>{t('panel.masters.holders')}</th>
            <th>{t('panel.masters.dns')}</th>
            <th class="n">{t('stats.col.clients')}</th>
          </tr>
        </thead>
        <tbody>
          {#each domains.data ?? [] as d (d.domain)}
            <tr class={d.masters.length > 0 ? 'mrow' : undefined}>
              <td class="full" data-l=""><span class="mono">{d.domain}</span></td>
              <td data-l={t('panel.masters.holders')}>
                {#if d.folded_into}
                  <span class="muted">{t('panel.masters.folded', { domain: shortDomain(d.folded_into) })}</span>
                {:else if d.masters.length === 0}
                  <span class="muted">{t('panel.masters.none')}</span>
                {:else}
                  {#each d.masters as id (id)}
                    {@const n = byId.get(id)}
                    {@const left = n ? secondsUntil(n.rotates_at, clock1.ms) : null}
                    <div>
                      <MasterTag />
                      <a class="nm" href={hrefOf('node', id)}>{id}</a>
                      {#if left !== null}<span class="muted">{t('node.role.rotates', { time: clock(left) })}</span>{/if}
                    </div>
                  {/each}
                {/if}
              </td>
              <td data-l={t('panel.masters.dns')}>
                {#if d.dns}<span class="mono">{d.dns.values.join(', ')}</span>{:else}—{/if}
                <span class="st {d.dns_synced ? 'ok' : 'bad'}">{d.dns_synced ? t('panel.masters.synced') : t('panel.masters.unsynced')}</span>
              </td>
              <td class="n" data-l={t('stats.col.clients')}>{d.clients ?? '—'}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  </div>
</div>
