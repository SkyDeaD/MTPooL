<script lang="ts">
  import type { Snippet } from 'svelte';
  import { messages, t, type Key } from './i18n';
  import { FIELDS, type Form, type Problem } from './settings-fields';
  import type { SettingsStore } from './settings-store.svelte';
  import Icon from './ui/Icon.svelte';

  // One group of settings in its own tile with its own save button. It sends only its fields, and
  // says by itself when something in it is unsaved.
  let {
    store,
    form,
    title,
    sub = '',
    extra,
  }: { store: SettingsStore; form: Form; title: string; sub?: string; extra?: Snippet } = $props();

  const all = $derived([...form.fields, ...(form.adv ?? [])]);
  const dirty = $derived(store.changed(all).length > 0);

  function problemText(p: Problem): string {
    if (p.code === 'int') return t('settings.error.int', { min: p.min ?? 1, max: p.max ?? 86400 });
    if (p.code === 'float') return t('settings.error.float', { min: p.min ?? 0, max: p.max ?? 1 });
    return t(`settings.error.${p.code}` as Key);
  }
  const hint = (key: string): string => (`settings.h.${key}` in messages ? t(`settings.h.${key}` as Key) : '');

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    await store.save(all);
  }
</script>

{#snippet field(key: string)}
  {@const f = FIELDS.get(key)!}
  {@const label = t(`settings.${key}` as Key)}
  {@const h = hint(key)}
  {@const err = store.errors[key]}
  <div class={['f', (f.kind === 'domains' || f.kind === 'users') && 'wide']}>
    {#if f.kind === 'bool'}
      <span class="lbl">{label}</span>
      <label class="sw">
        <input type="checkbox" data-key={key} checked={store.values[key] === true} onchange={(e) => (store.values[key] = e.currentTarget.checked)} />
        <span class="k"></span>
        <span class="sm">{store.values[key] === true ? t('settings.on') : t('settings.off')}</span>
      </label>
    {:else if f.kind === 'domains' || f.kind === 'users'}
      <label for={`f-${key}`}>{label}</label>
      <textarea
        class={['inp ta mono', err && 'err']}
        id={`f-${key}`}
        rows="3"
        spellcheck="false"
        data-key={key}
        value={store.values[key] as string}
        oninput={(e) => (store.values[key] = e.currentTarget.value)}
      ></textarea>
    {:else}
      <label for={`f-${key}`}>{label}</label>
      <div class="u">
        <input
          class={['inp', f.kind === 'text' && 'mono', err && 'err']}
          id={`f-${key}`}
          type={f.kind === 'text' ? 'text' : 'number'}
          step={f.kind === 'ratio' ? '0.05' : undefined}
          inputmode={f.kind === 'text' ? undefined : f.kind === 'ratio' ? 'decimal' : 'numeric'}
          spellcheck="false"
          autocomplete="off"
          data-key={key}
          value={store.values[key] as string}
          oninput={(e) => (store.values[key] = e.currentTarget.value)}
        />
        {#if f.unit === 'min'}<span>{t('settings.unit.min')}</span>{:else if f.kind === 'sec'}<span>{t('settings.unit.sec')}</span>{:else if f.kind === 'percent'}<span>{t('settings.unit.percent')}</span>{/if}
      </div>
    {/if}
    {#if h}<span class="h">{h}</span>{/if}
    {#if err}<span class="e"><Icon name="alert" /><span>{problemText(err)}</span></span>{/if}
  </div>
{/snippet}

<form class="settings t" data-form={form.id} onsubmit={submit} novalidate>
  <div class="th">
    <h2>{title}</h2>
    {#if sub}<span class="sub">{sub}</span>{/if}
  </div>
  <div class="fg">
    {#each form.fields as key (key)}{@render field(key)}{/each}
  </div>
  {#if form.adv}
    <details class="adv">
      <summary><Icon name="chev" cls="chev" />{t('settings.advanced')}</summary>
      <div class="fg">
        {#each form.adv as key (key)}{@render field(key)}{/each}
      </div>
    </details>
  {/if}
  {@render extra?.()}
  <div class="ff">
    <button class="btn pri" type="submit" disabled={!dirty || store.saving}>{t('common.save')}</button>
    {#if dirty}<span class="faint sm" role="status">{t('settings.unsaved')}</span>{/if}
  </div>
</form>
