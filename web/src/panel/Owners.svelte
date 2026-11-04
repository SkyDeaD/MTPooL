<script lang="ts">
  import { ApiError } from '../lib/api';
  import { formatWhen } from '../lib/dates';
  import { t } from './i18n';
  import { toast } from '../lib/toast.svelte';
  import type { Key as MessageKey } from './i18n';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import { usePolled } from './live.svelte';
  import type { NewTokenCreated, Owner, TokenMeta } from './types';
  import Choice from './ui/Choice.svelte';
  import Confirm from './ui/Confirm.svelte';
  import Icon from './ui/Icon.svelte';
  import Modal from './ui/Modal.svelte';
  import { Popover } from 'bits-ui';

  // The people whose servers join the pool, and the tokens that let them and the bots in.
  const panel = usePanel();
  const owners = usePolled<Owner[]>(() => '/api/v1/admin/owners');
  const tokens = usePolled<TokenMeta[]>(() => '/api/v1/admin/tokens');

  const ownerName = (id: number | null | undefined) => owners.data?.find((o) => o.id === id)?.name ?? '';
  const kindText = (kind: string) => (['admin', 'enroll', 'node', 'bot', 'metrics'].includes(kind) ? t(`panel.tokens.kind.${kind}` as MessageKey) : kind);

  // Owners.
  let ownerOpen = $state(false);
  let editing = $state<Owner | null>(null);
  let formName = $state('');
  let formLimit = $state('10');
  let formNote = $state('');
  let ownerBusy = $state(false);

  function newOwner(): void {
    editing = null;
    formName = '';
    formLimit = '10';
    formNote = '';
    ownerOpen = true;
  }
  function editOwner(o: Owner): void {
    editing = o;
    formName = o.name;
    formLimit = String(o.node_limit);
    formNote = o.note;
    ownerOpen = true;
  }
  const limitOk = $derived(/^\d+$/.test(formLimit.trim()));
  async function saveOwner(): Promise<void> {
    if (!formName.trim() || !limitOk) return;
    ownerBusy = true;
    const body = { name: formName.trim(), node_limit: Number(formLimit), note: formNote.trim() };
    const ok = editing
      ? await act(panel, () => panel.client.patch('/api/v1/admin/owners/{id}', { params: { id: editing!.id }, body }), t('panel.owners.saved'))
      : await act(panel, () => panel.client.post('/api/v1/admin/owners', { body }), t('panel.owners.created'));
    ownerBusy = false;
    if (ok) ownerOpen = false;
  }

  let deleting = $state<Owner | null>(null);
  async function removeOwner(): Promise<void> {
    if (!deleting) return;
    const id = deleting.id;
    await act(panel, () => panel.client.delete('/api/v1/admin/owners/{id}', { params: { id } }), t('panel.owners.deleted'));
    deleting = null;
  }

  // Issuing a token: the value comes once, in this dialog, and is not kept anywhere else.
  let issueOpen = $state(false);
  let kind = $state('enroll');
  let ownerId = $state('');
  let life = $state('never');
  let created = $state<NewTokenCreated | null>(null);
  let issueBusy = $state(false);
  const needsOwner = $derived(kind !== 'metrics');

  function openIssue(): void {
    kind = 'enroll';
    ownerId = String(owners.data?.[0]?.id ?? '');
    life = 'never';
    created = null;
    issueOpen = true;
  }
  $effect(() => {
    if (!issueOpen) created = null;
  });

  async function issue(): Promise<void> {
    issueBusy = true;
    const body: Record<string, unknown> = { kind };
    if (needsOwner) body.owner_id = Number(ownerId);
    if (life !== 'never') body.expires_at = new Date(Date.now() + Number(life) * 86400_000).toISOString();
    try {
      created = (await panel.client.post('/api/v1/admin/tokens', { body: body as never })) as unknown as NewTokenCreated;
      panel.refreshAll();
    } catch (error) {
      toast(error instanceof ApiError ? `${error.message}${error.detail ? `: ${error.detail}` : ''}` : t('error.network'), 5000);
    }
    issueBusy = false;
  }
  const secret = $derived((created as unknown as { token?: string } | null)?.token ?? '');
  let copied = $state(false);
  async function copy(): Promise<void> {
    try {
      await navigator.clipboard.writeText(secret);
      copied = true;
    } catch {
      toast(t('panel.tokens.copyFailed'));
    }
  }
  $effect(() => {
    if (!issueOpen) copied = false;
  });

  let revoking = $state<TokenMeta | null>(null);
  async function revoke(): Promise<void> {
    if (!revoking) return;
    const id = revoking.id;
    await act(panel, () => panel.client.delete('/api/v1/admin/tokens/{id}', { params: { id } }), t('panel.revoke.done'));
    revoking = null;
  }

  const kinds = (['enroll', 'bot', 'metrics'] as const).map((k) => ({ value: k, label: t(`panel.tokens.kind.${k}`) }));
  const lives = [
    { value: 'never', label: t('panel.tokens.life.never') },
    { value: '30', label: t('panel.tokens.life.30') },
    { value: '90', label: t('panel.tokens.life.90') },
  ];
  const ownerItems = $derived((owners.data ?? []).map((o) => ({ value: String(o.id), label: o.name })));
</script>

<div class="view stack">
  <header class="ph">
    <h1>{t('panel.owners.title')}</h1>
    <span class="sub">{t('panel.owners.sub')}</span>
    <span class="sp"></span>
    <button class="btn pri" type="button" onclick={openIssue}><Icon name="key" />{t('panel.tokens.issue')}</button>
  </header>
  {#if owners.stale || tokens.stale}<p class="note" role="status">{t('common.stale')}</p>{/if}

  <section class="t">
    <div class="th">
      <h2>{t('panel.owners.list')}</h2>
      <span class="sp"></span>
      <button class="btn s" type="button" onclick={newOwner}><Icon name="plus" />{t('panel.owners.add')}</button>
    </div>
    <div class="tw">
      <table class="tb list">
        <thead>
          <tr>
            <th>{t('panel.owners.name')}</th>
            <th class="n">{t('panel.owners.limit')}</th>
            <th>{t('panel.owners.note')}</th>
            <th><span class="sr-only">{t('panel.table.actions')}</span></th>
          </tr>
        </thead>
        <tbody>
          {#each owners.data ?? [] as o (o.id)}
            <tr>
              <td class="full" data-l="">{o.name}</td>
              <td class="n" data-l={t('panel.owners.limit')}>{o.node_limit}</td>
              <td data-l={t('panel.owners.note')}>{o.note || '—'}</td>
              <td class="n">
                <Popover.Root>
                  <Popover.Trigger class="btn s ic txt" aria-label={`${t('panel.table.actions')}: ${o.name}`}><Icon name="more" /></Popover.Trigger>
                  <Popover.Portal>
                    <Popover.Content class="pop" align="end" sideOffset={4}>
                      <button type="button" onclick={() => editOwner(o)}>{t('panel.owners.edit')}</button>
                      <button type="button" class="dng" onclick={() => (deleting = o)}>{t('panel.owners.delete')}</button>
                    </Popover.Content>
                  </Popover.Portal>
                </Popover.Root>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>

  <section class="t">
    <div class="th">
      <h2>{t('panel.tokens.title')}</h2>
      <span class="sub">{t('panel.tokens.sub')}</span>
    </div>
    <div class="tw">
      <table class="tb list">
        <thead>
          <tr>
            <th>{t('panel.tokens.prefix')}</th>
            <th>{t('panel.tokens.kindCol')}</th>
            <th>{t('panel.tokens.owner')}</th>
            <th>{t('panel.tokens.expires')}</th>
            <th>{t('panel.tokens.lastUsed')}</th>
            <th><span class="sr-only">{t('panel.table.actions')}</span></th>
          </tr>
        </thead>
        <tbody>
          {#each tokens.data ?? [] as k (k.id)}
            <tr class={k.revoked_at && 'off'}>
              <td class="full" data-l=""><span class="mono">{k.prefix}</span></td>
              <td data-l={t('panel.tokens.kindCol')}>{kindText(k.kind)}</td>
              <td data-l={t('panel.tokens.owner')}>{ownerName(k.owner_id) || '—'}</td>
              <td data-l={t('panel.tokens.expires')}>
                {#if k.revoked_at}{t('panel.tokens.revoked')}{:else if k.expires_at}{formatWhen(Date.parse(k.expires_at))}{:else}{t('panel.tokens.life.never')}{/if}
              </td>
              <td data-l={t('panel.tokens.lastUsed')}>{k.last_used_at ? formatWhen(Date.parse(k.last_used_at)) : '—'}</td>
              <td class="n">
                {#if !k.revoked_at}
                  <button class="btn s dng" type="button" data-revoke onclick={() => (revoking = k)}>{t('panel.tokens.revoke')}</button>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>
</div>

<Modal bind:open={ownerOpen} title={editing ? t('panel.owners.editTitle') : t('panel.owners.addTitle')}>
  <div class="stack">
    <div class="f">
      <label for="o-name">{t('panel.owners.name')}</label>
      <input class="inp" id="o-name" bind:value={formName} />
    </div>
    <div class="f">
      <label for="o-limit">{t('panel.owners.limit')}</label>
      <input class="inp" id="o-limit" inputmode="numeric" bind:value={formLimit} />
    </div>
    <div class="f">
      <label for="o-note">{t('panel.owners.note')}</label>
      <input class="inp" id="o-note" bind:value={formNote} />
    </div>
  </div>
  {#snippet footer()}
    <button class="btn" type="button" onclick={() => (ownerOpen = false)}>{t('common.cancel')}</button>
    <button class="btn pri" type="button" disabled={ownerBusy || !formName.trim() || !limitOk} onclick={saveOwner}>{t('common.save')}</button>
  {/snippet}
</Modal>

<Confirm
  bind:open={() => deleting !== null, (v) => { if (!v) deleting = null; }}
  title={t('panel.owners.deleteTitle', { name: deleting?.name ?? '' })}
  text={t('panel.owners.deleteText')}
  keep={t('panel.owners.deleteKeep')}
  confirm={t('panel.owners.deleteConfirm')}
  onconfirm={removeOwner}
/>

<Confirm
  bind:open={() => revoking !== null, (v) => { if (!v) revoking = null; }}
  title={t('panel.revoke.title', { prefix: revoking?.prefix ?? '' })}
  text={t('panel.revoke.text')}
  keep={t('panel.revoke.keep')}
  confirm={t('panel.revoke.confirm')}
  onconfirm={revoke}
/>

<Modal bind:open={issueOpen} title={created ? t('panel.tokens.createdTitle') : t('panel.tokens.issue')}>
  {#if created}
    <p class="muted">{t('panel.tokens.once')}</p>
    <pre class="code">{secret}</pre>
    <div class="row-actions">
      <button class="btn" type="button" onclick={copy}>{t('panel.tokens.copy')}</button>
      {#if copied}<span class="muted" role="status">{t('panel.tokens.copied')}</span>{/if}
    </div>
  {:else}
    <div class="stack">
      <div class="f">
        <span class="lbl">{t('panel.tokens.kindCol')}</span>
        <Choice bind:value={kind} items={kinds} label={t('panel.tokens.kindCol')} />
      </div>
      {#if needsOwner}
        <div class="f">
          <span class="lbl">{t('panel.tokens.owner')}</span>
          <Choice bind:value={ownerId} items={ownerItems} label={t('panel.tokens.owner')} />
        </div>
      {/if}
      <div class="f">
        <span class="lbl">{t('panel.tokens.expires')}</span>
        <Choice bind:value={life} items={lives} label={t('panel.tokens.expires')} />
      </div>
    </div>
  {/if}
  {#snippet footer()}
    {#if created}
      <button class="btn pri" type="button" onclick={() => (issueOpen = false)}>{t('panel.tokens.saved')}</button>
    {:else}
      <button class="btn" type="button" onclick={() => (issueOpen = false)}>{t('common.cancel')}</button>
      <button class="btn pri" type="button" disabled={issueBusy || (needsOwner && !ownerId)} onclick={issue}>{t('panel.tokens.create')}</button>
    {/if}
  {/snippet}
</Modal>
