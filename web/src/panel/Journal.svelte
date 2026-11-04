<script lang="ts">
  import { Tabs } from 'bits-ui';
  import { formatWhen } from '../lib/dates';
  import { t } from '../lib/i18n';
  import { messages, type MessageKey } from '../strings.ru';
  import { usePolled } from './live.svelte';
  import { go, hrefOf } from './route.svelte';
  import type { AuditEntry, EventEntry } from './types';

  // The pool's events and, in the second tab, what the admins did.
  let { tab }: { tab?: string } = $props();

  const events = usePolled<EventEntry[]>(() => '/api/v1/admin/events');
  const audit = usePolled<AuditEntry[]>(() => '/api/v1/admin/audit');
  const active = $derived(tab === 'audit' ? 'audit' : 'events');

  const word = (prefix: string, key: string) => {
    const full = `${prefix}.${key}`;
    return full in messages ? t(full as MessageKey) : key;
  };
  /** The facts of an entry as "name: value", for the ones that are plain values. */
  const facts = (detail: unknown): string =>
    Object.entries((detail ?? {}) as Record<string, unknown>)
      .filter(([, v]) => ['string', 'number', 'boolean'].includes(typeof v))
      .map(([k, v]) => `${word('panel.journal.field', k)}: ${v}`)
      .join(', ');
  const fields = (detail: unknown): string => {
    const list = (detail as { fields?: unknown } | null)?.fields;
    return Array.isArray(list) ? list.map((f) => t(`settings.${String(f)}` as MessageKey)).join(', ') : '';
  };
</script>

<div class="view stack">
  <header class="ph"><h1>{t('panel.journal.title')}</h1></header>

  <Tabs.Root value={active} onValueChange={(v) => go('journal', v === 'audit' ? 'audit' : undefined)}>
    <Tabs.List class="tabs" aria-label={t('panel.journal.title')}>
      <Tabs.Trigger value="events">{t('panel.journal.events')}</Tabs.Trigger>
      <Tabs.Trigger value="audit">{t('panel.journal.audit')}</Tabs.Trigger>
    </Tabs.List>

    <Tabs.Content value="events">
      {#if events.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}
      <div class="t">
        <div class="tw">
          <table class="tb list">
            <thead>
              <tr>
                <th>{t('panel.journal.when')}</th>
                <th>{t('panel.journal.what')}</th>
                <th>{t('panel.journal.who')}</th>
                <th>{t('panel.journal.details')}</th>
              </tr>
            </thead>
            <tbody>
              {#each events.data ?? [] as e (e.id)}
                <tr>
                  <td data-l={t('panel.journal.when')}>{formatWhen(Date.parse(e.at))}</td>
                  <td data-l={t('panel.journal.what')}>{word('panel.event', e.kind)}</td>
                  <td data-l={t('panel.journal.who')}>
                    {#if e.node}<a class="nm" href={hrefOf('node', e.node)}>{e.node}</a>{/if}
                    {#if e.domain}<span class="mono">{e.domain}</span>{/if}
                  </td>
                  <td data-l={t('panel.journal.details')}>{facts(e.detail)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        {#if events.settled && (events.data ?? []).length === 0}<div class="tb-b muted">{t('panel.journal.none')}</div>{/if}
      </div>
    </Tabs.Content>

    <Tabs.Content value="audit">
      {#if audit.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}
      <div class="t">
        <div class="tw">
          <table class="tb list">
            <thead>
              <tr>
                <th>{t('panel.journal.when')}</th>
                <th>{t('panel.journal.action')}</th>
                <th>{t('panel.journal.target')}</th>
                <th>{t('panel.journal.details')}</th>
              </tr>
            </thead>
            <tbody>
              {#each audit.data ?? [] as a (a.id)}
                <tr>
                  <td data-l={t('panel.journal.when')}>{formatWhen(Date.parse(a.at))}</td>
                  <td data-l={t('panel.journal.action')}>{word('panel.audit', a.action)}</td>
                  <td data-l={t('panel.journal.target')}>{a.target ?? ''}</td>
                  <td data-l={t('panel.journal.details')}>{fields(a.detail) || facts(a.detail)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
        {#if audit.settled && (audit.data ?? []).length === 0}<div class="tb-b muted">{t('panel.journal.none')}</div>{/if}
      </div>
    </Tabs.Content>
  </Tabs.Root>
</div>
