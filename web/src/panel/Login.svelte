<script lang="ts">
  import { ApiError } from '../lib/api';
  import { t } from './i18n';
  import Logo from '../shared/Logo.svelte';
  import { usePanel } from './context.svelte';
  import Icon from './ui/Icon.svelte';

  // The admin token is typed here, sent once to open a session, and forgotten. It is not kept in
  // the address, in storage, or in the page after the answer.
  let { expired = false, onlogin }: { expired?: boolean; onlogin: () => void } = $props();

  const panel = usePanel();
  let token = $state('');
  let busy = $state(false);
  let shown = $state(false);
  let error = $state<string | null>(null);

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (busy || !token) return;
    busy = true;
    error = null;
    try {
      await panel.client.login(token);
      token = '';
      onlogin();
    } catch (e) {
      error = e instanceof ApiError && e.status === 401 ? t('panel.login.error') : t('error.network');
    } finally {
      busy = false;
    }
  }
</script>

<main class="login">
  <div class="t">
    <form onsubmit={submit}>
      <Logo href="#/" />
      <h1>{t('panel.login.title')}</h1>
      <p class="muted">{t('panel.login.lead')}</p>
      {#if expired}<p class="note" role="status">{t('panel.login.expired')}</p>{/if}
      <div class="f">
        <label for="token">{t('panel.login.token')}</label>
        <div class="u">
          <input
            id="token"
            class={['inp', error && 'err']}
            name="token"
            type={shown ? 'text' : 'password'}
            autocomplete="off"
            spellcheck="false"
            bind:value={token}
          />
          <button class="btn s ic txt" type="button" aria-label={t('panel.login.show')} aria-pressed={shown} onclick={() => (shown = !shown)}>
            <Icon name="eye" />
          </button>
        </div>
        {#if error}<div class="e" role="alert">{error}</div>{/if}
        <div class="h">{t('panel.login.hint')}</div>
      </div>
      <button class="btn pri" type="submit" disabled={busy}>{t('panel.login.submit')}</button>
      <a class="sm" href="/">{t('panel.login.public')}</a>
    </form>
  </div>
</main>
