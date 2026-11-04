<script lang="ts">
  import { useNow } from '../lib/clock.svelte';
  import { dayOf, formatWhen } from '../lib/dates';
  import EventRow from './EventRow.svelte';
  import { CATEGORIES, describeAudit, describeEvent, type Category } from './events';
  import { t, type Key } from './i18n';
  import { usePolled } from './live.svelte';
  import { hrefOf } from './route.svelte';
  import type { AuditEntry, EventEntry } from './types';
  import Icon from './ui/Icon.svelte';

  // The pool's events with a filter and a search and, in the second tab, what people did.
  let { tab }: { tab?: string } = $props();

  const events = usePolled<EventEntry[]>(() => '/api/v1/admin/events?limit=200');
  const audit = usePolled<AuditEntry[]>(() => '/api/v1/admin/audit');
  const clock = useNow(1000);
  const active = $derived(tab === 'audit' ? 'audit' : 'events');
  let query = $state('');
  let filter = $state<Category | 'all'>('all');

  const described = $derived((events.data ?? []).map((event) => ({ event, d: describeEvent(event) })));
  const counts = $derived.by(() => {
    const c: Record<string, number> = { all: described.length };
    for (const k of CATEGORIES) c[k] = described.filter((x) => x.d.category === k).length;
    return c;
  });
  const shown = $derived.by(() => {
    const q = query.trim().toLowerCase();
    return described.filter((x) => (filter === 'all' || x.d.category === filter) && (!q || x.d.haystack.includes(q)));
  });
  // The list is newest first; a separator goes before the first entry of each day.
  const dayKey = (iso: string) => dayOf(Date.parse(iso));
  function dayLabel(iso: string): string {
    const day = dayKey(iso);
    if (day === dayOf(clock.ms)) return t('panel.journal.today');
    if (day === dayOf(clock.ms - 86_400_000)) return t('panel.journal.yesterday');
    return day;
  }
  const who = (actor: string) => (actor.startsWith('token:') ? t('panel.journal.byToken', { id: actor.slice(6) }) : t('panel.journal.bySession'));
</script>

<div class="view stack">
  <header class="ph">
    <h1>{t('panel.journal.title')}</h1>
    <span class="sub">{t('panel.journal.sub')}</span>
  </header>

  <div class="tabs" role="tablist" aria-label={t('panel.journal.title')}>
    <a role="tab" href={hrefOf('journal')} aria-selected={active === 'events'}>{t('panel.journal.events')}</a>
    <a role="tab" href={hrefOf('journal', 'audit')} aria-selected={active === 'audit'}>{t('panel.journal.audit')}</a>
  </div>

  {#if active === 'events'}
    {#if events.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}
    <section class="t">
      <div class="th">
        <div class="srch">
          <Icon name="search" />
          <input class="inp" type="search" bind:value={query} placeholder={t('panel.journal.search')} aria-label={t('panel.journal.search')} />
        </div>
        <div class="chips">
          {#each ['all', ...CATEGORIES] as const as k (k)}
            <button class="chip" type="button" aria-pressed={filter === k} onclick={() => (filter = k)}>
              {t(`panel.journal.cat.${k}` as Key)} <b>{counts[k]}</b>
            </button>
          {/each}
        </div>
      </div>
      {#each shown as x, i (x.event.id)}
        {#if i === 0 || dayKey(shown[i - 1].event.at) !== dayKey(x.event.at)}
          <div class="day">{dayLabel(x.event.at)}</div>
        {/if}
        <EventRow event={x.event} now={clock.ms} />
      {:else}
        {#if events.settled}<div class="tb-b"><p class="muted">{t('panel.journal.noMatch')}</p></div>{/if}
      {/each}
    </section>
  {:else}
    {#if audit.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}
    <section class="t">
      <div class="tw">
        <table class="tb list">
          <thead>
            <tr>
              <th>{t('panel.journal.when')}</th>
              <th>{t('panel.journal.who')}</th>
              <th>{t('panel.journal.action')}</th>
              <th>{t('panel.journal.target')}</th>
            </tr>
          </thead>
          <tbody>
            {#each audit.data ?? [] as a (a.id)}
              {@const d = describeAudit(a)}
              <tr>
                <td data-l={t('panel.journal.when')}>{formatWhen(Date.parse(a.at))}</td>
                <td data-l={t('panel.journal.who')}>{who(a.actor)}</td>
                <td class="full" data-l={t('panel.journal.action')}>{d.what}</td>
                <td class="mono sm" data-l={t('panel.journal.target')}>{d.target || '—'}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if audit.settled && (audit.data ?? []).length === 0}<div class="tb-b muted">{t('panel.journal.none')}</div>{/if}
    </section>
  {/if}
</div>
