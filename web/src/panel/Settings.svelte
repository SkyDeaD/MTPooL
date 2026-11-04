<script lang="ts">
  import { ApiError } from '../lib/api';
  import { t } from '../lib/i18n';
  import { toast } from '../lib/toast.svelte';
  import type { MessageKey } from '../strings.ru';
  import { usePanel } from './context.svelte';
  import { GROUPS, SECRETS, parse, pick, put, seed, type Field, type Problem, type Values } from './settings-fields';
  import type { SettingsDoc } from './types';
  import Modal from './ui/Modal.svelte';

  // Every setting the server has, in words and in seconds, saved with one button. Secrets are
  // write-only: the server says whether one is set, never what it is.
  const panel = usePanel();

  let doc = $state.raw<SettingsDoc | null>(null);
  let values = $state<Values>({});
  let loadFailed = $state(false);
  let saving = $state(false);
  let errors = $state<Record<string, Problem>>({});

  const original = $derived(doc ? seed(doc) : {});
  const changed = $derived(Object.keys(values).filter((k) => values[k] !== original[k]));
  const dirty = $derived(changed.length > 0);

  async function load(): Promise<void> {
    try {
      doc = (await panel.client.get('/api/v1/admin/settings')) as unknown as SettingsDoc;
      values = seed(doc);
    } catch {
      loadFailed = true;
    }
  }
  $effect(() => {
    void load();
  });

  // Closing the tab with unsaved changes asks first.
  $effect(() => {
    const guard = (event: BeforeUnloadEvent) => {
      if (!dirty) return;
      event.preventDefault();
      event.returnValue = '';
    };
    window.addEventListener('beforeunload', guard);
    return () => window.removeEventListener('beforeunload', guard);
  });

  const fields = GROUPS.flatMap((g) => g.fields);
  const byKey = new Map(fields.map((f) => [f.key, f]));

  function problemText(p: Problem): string {
    if (p.code === 'int') return t('settings.error.int', { min: p.min ?? 1, max: p.max ?? 86400 });
    if (p.code === 'float') return t('settings.error.float', { min: p.min ?? 0, max: p.max ?? 1 });
    return t(`settings.error.${p.code}` as MessageKey);
  }

  async function write(patch: Record<string, unknown>): Promise<boolean> {
    try {
      doc = (await panel.client.patch('/api/v1/admin/settings', { body: patch as never })) as unknown as SettingsDoc;
      panel.refreshAll();
      return true;
    } catch (error) {
      toast(error instanceof ApiError ? `${error.message}${error.detail ? `: ${error.detail}` : ''}` : t('error.network'), 5000);
      return false;
    }
  }

  async function save(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (!dirty || saving) return;
    const patch: Record<string, unknown> = {};
    const found: Record<string, Problem> = {};
    for (const key of changed) {
      const parsed = parse(byKey.get(key) as Field, values[key]);
      if (parsed.ok) put(patch, key, parsed.value);
      else found[key] = parsed.problem;
    }
    errors = found;
    if (Object.keys(found).length > 0) return;
    saving = true;
    if (await write(patch) && doc) {
      values = seed(doc);
      toast(t('settings.saved'));
    }
    saving = false;
  }

  // Secrets.
  let replacing = $state<(typeof SECRETS)[number] | null>(null);
  let secretOpen = $state(false);
  let secretValue = $state('');
  $effect(() => {
    secretOpen = replacing !== null;
  });
  $effect(() => {
    if (!secretOpen && replacing !== null) {
      replacing = null;
      secretValue = '';
    }
  });

  async function saveSecret(): Promise<void> {
    if (replacing === null || secretValue === '') return;
    const name = replacing;
    const ok = await write({ secrets: { [name]: secretValue } });
    if (ok) {
      toast(t('settings.secret.saved'));
      secretOpen = false;
    }
    secretValue = '';
  }
  async function clearSecret(name: string): Promise<void> {
    if (await write({ secrets: { [name]: null } })) toast(t('settings.secret.cleared'));
  }
</script>

<div class="view stack">
  <header class="ph"><h1>{t('panel.settings.title')}</h1></header>

  {#if loadFailed}
    <p class="muted">{t('error.network')}</p>
  {:else if !doc}
    <p class="muted">{t('common.loading')}</p>
  {:else}
    <form class="settings t" onsubmit={save} novalidate>
      {#each GROUPS as group (group.id)}
        <div class="sect">{t(`settings.group.${group.id}` as MessageKey)}</div>
        <div class="fg">
          {#each group.fields as field (field.key)}
            {@const label = t(`settings.${field.key}` as MessageKey)}
            <div class={['f', (field.kind === 'domains' || field.kind === 'users') && 'wide']}>
              {#if field.kind === 'bool'}
                <label class="sw">
                  <input type="checkbox" data-key={field.key} checked={values[field.key] === true} onchange={(e) => (values[field.key] = e.currentTarget.checked)} />
                  <span class="k"></span>
                  <span>{label}</span>
                </label>
              {:else if field.kind === 'domains' || field.kind === 'users'}
                <label for={`f-${field.key}`}>{label}</label>
                <textarea
                  class={['inp ta', errors[field.key] && 'err']}
                  id={`f-${field.key}`}
                  rows="4"
                  data-key={field.key}
                  value={values[field.key] as string}
                  oninput={(e) => (values[field.key] = e.currentTarget.value)}
                ></textarea>
                <div class="h">{t(field.kind === 'domains' ? 'settings.hint.domains' : 'settings.hint.users')}</div>
              {:else}
                <label for={`f-${field.key}`}>{label}</label>
                <div class="u">
                  <input
                    class={['inp', errors[field.key] && 'err']}
                    id={`f-${field.key}`}
                    type={field.kind === 'text' ? 'text' : 'number'}
                    step={field.kind === 'ratio' ? '0.05' : undefined}
                    inputmode={field.kind === 'text' ? undefined : field.kind === 'ratio' ? 'decimal' : 'numeric'}
                    data-key={field.key}
                    value={values[field.key] as string}
                    oninput={(e) => (values[field.key] = e.currentTarget.value)}
                  />
                  {#if field.kind === 'sec'}<span>{t('settings.unit.sec')}</span>{/if}
                  {#if field.kind === 'percent'}<span>{t('settings.unit.percent')}</span>{/if}
                </div>
              {/if}
              {#if errors[field.key]}<div class="e">{problemText(errors[field.key])}</div>{/if}
            </div>
          {/each}
        </div>
      {/each}

      <div class="sect">{t('settings.group.secrets')}</div>
      <div class="fg">
        {#each SECRETS as name (name)}
          {@const isSet = doc.secrets[name] === true}
          <div class="f wide secret">
            <div class="state">
              <div>{t(`settings.secret.${name}` as MessageKey)}</div>
              <div class="h">{isSet ? t('settings.secret.set') : t('settings.secret.unset')}</div>
            </div>
            <button class="btn s" type="button" onclick={() => (replacing = name)}>{t('settings.secret.replace')}</button>
            {#if isSet}<button class="btn s dng" type="button" onclick={() => clearSecret(name)}>{t('settings.secret.clear')}</button>{/if}
          </div>
        {/each}
      </div>

      <div class="ff savebar">
        <button class="btn pri" type="submit" disabled={!dirty || saving}>{t('common.save')}</button>
        {#if dirty}<span class="muted" role="status">{t('settings.unsaved')}</span>{/if}
      </div>
    </form>
  {/if}
</div>

<Modal bind:open={secretOpen} title={replacing ? t('settings.secret.title', { label: t(`settings.secret.${replacing}` as MessageKey) }) : ''}>
  <div class="f">
    <label for="secret-new">{t('settings.secret.value')}</label>
    <input class="inp secret-in" id="secret-new" type="text" autocomplete="off" autocapitalize="off" spellcheck="false" bind:value={secretValue} />
    <div class="h">{t('settings.secret.hint')}</div>
  </div>
  {#snippet footer()}
    <button class="btn" type="button" onclick={() => (secretOpen = false)}>{t('common.cancel')}</button>
    <button class="btn pri" type="button" disabled={secretValue === ''} onclick={saveSecret}>{t('common.save')}</button>
  {/snippet}
</Modal>
