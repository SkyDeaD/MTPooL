<script lang="ts">
  import { ApiError } from '../lib/api';
  import { t } from '../lib/i18n';
  import Logo from '../shared/Logo.svelte';
  import { usePanel } from './context.svelte';

  // The admin token is typed here, sent once to open a session, and forgotten. It is not kept in
  // the address, in storage, or in the page after the answer.
  let { expired = false, onlogin }: { expired?: boolean; onlogin: () => void } = $props();

  const panel = usePanel();
  let token = $state('');
  let busy = $state(false);
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
    <h1>{t('panel.login.title')}</h1>
    <form onsubmit={submit}>
      <Logo href="#/" />
      {#if expired}<p class="note" role="status">{t('panel.login.expired')}</p>{/if}
      <div class="f">
        <label for="token">{t('panel.login.token')}</label>
        <input
          id="token"
          class={['inp', error && 'err']}
          name="token"
          type="password"
          autocomplete="off"
          spellcheck="false"
          bind:value={token}
        />
        {#if error}<div class="e" role="alert">{error}</div>{/if}
        <div class="h">{t('panel.login.hint')}</div>
      </div>
      <button class="btn pri" type="submit" disabled={busy}>{t('panel.login.submit')}</button>
    </form>
  </div>
</main>
