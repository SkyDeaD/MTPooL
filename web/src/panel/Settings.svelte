<script lang="ts">
  import { onMount } from 'svelte';
  import { toast } from '../lib/toast.svelte';
  import { usePanel } from './context.svelte';
  import { t, type Key } from './i18n';
  import { copyText, installCommand } from './install';
  import { guard } from './route.svelte';
  import { FORMS, SECRETS, TABS, type Tab } from './settings-fields';
  import { createSettings } from './settings-store.svelte';
  import SettingsForm from './SettingsForm.svelte';
  import Confirm from './ui/Confirm.svelte';
  import Icon from './ui/Icon.svelte';
  import Modal from './ui/Modal.svelte';

  // Every setting the server has, in words, in groups, each group saved on its own. Secrets are
  // write-only: the server says whether one is set, never what it is.
  const panel = usePanel();
  const store = createSettings(panel);
  let tab = $state<Tab>('connect');

  onMount(() => {
    void store.load();
  });

  const allKeys = Object.values(FORMS)
    .filter((f) => f.id !== 'sr')
    .flatMap((f) => [...f.fields, ...(f.adv ?? [])]);
  const dirty = $derived(store.changed(allKeys).length > 0);
  $effect(() => {
    guard.dirty = dirty;
    return () => {
      guard.dirty = false;
    };
  });
  // Closing the tab with unsaved changes asks first.
  $effect(() => {
    const warn = (event: BeforeUnloadEvent) => {
      if (!dirty) return;
      event.preventDefault();
      event.returnValue = '';
    };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  });

  const command = $derived(installCommand());
  async function copy(): Promise<void> {
    toast((await copyText(command)) ? t('panel.tokens.copied') : t('panel.tokens.copyFailed'));
  }

  // Secrets.
  let replacing = $state<(typeof SECRETS)[number] | null>(null);
  let secretValue = $state('');
  const secretLabel = (name: string) => t(`settings.secret.${name}` as Key);

  async function saveSecret(): Promise<void> {
    if (replacing === null || secretValue === '') return;
    const name = replacing;
    if (await store.write({ secrets: { [name]: secretValue } })) {
      toast(t('settings.secret.saved'));
      replacing = null;
      secretValue = '';
    }
  }
  async function clearSecret(name: string): Promise<void> {
    if (await store.write({ secrets: { [name]: null } })) toast(t('settings.secret.cleared'));
  }

  // Proxy users: a replaced secret is a new random 32-hex string, written with the whole list.
  type ProxyUser = { name: string; secret_hex: string };
  const users = $derived(((store.doc?.proxy_users ?? []) as ProxyUser[]).filter((u) => typeof u.name === 'string'));
  let renewing = $state<string | null>(null);
  let busy = $state(false);
  const newSecret = () => Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, '0')).join('');
  async function renew(): Promise<void> {
    if (renewing === null) return;
    busy = true;
    const list = users.map((u) => (u.name === renewing ? { ...u, secret_hex: newSecret() } : u));
    if (await store.write({ proxy_users: list })) toast(t('panel.settings.userRenewed', { name: renewing }));
    busy = false;
    renewing = null;
  }
  const masked = (hex: string) => `••••${hex.slice(-4)}`;
</script>

{#snippet secretRow(name: (typeof SECRETS)[number])}
  {@const isSet = store.doc?.secrets && (store.doc.secrets as Record<string, boolean>)[name] === true}
  <div class="f wide secret">
    <div class="state">
      <div>{secretLabel(name)}</div>
      <div class="h">{isSet ? t('settings.secret.set') : t('settings.secret.unset')}</div>
      <div class="h">{t(`settings.h.${name}` as Key)}</div>
    </div>
    <button class="btn s" type="button" onclick={() => (replacing = name)}>{t('settings.secret.replace')}</button>
    {#if isSet}<button class="btn s dng" type="button" onclick={() => clearSecret(name)}>{t('settings.secret.clear')}</button>{/if}
  </div>
{/snippet}

<div class="view stack">
  <header class="ph">
    <h1>{t('panel.settings.title')}</h1>
    <span class="sub">{t('panel.settings.sub')}</span>
  </header>

  {#if store.failed}
    <p class="muted">{t('error.network')}</p>
  {:else if !store.doc}
    <p class="muted">{t('common.loading')}</p>
  {:else}
    <div class="tabs" role="tablist" aria-label={t('panel.settings.title')}>
      {#each TABS as k (k)}
        <button type="button" role="tab" id={`stab-${k}`} aria-selected={tab === k} aria-controls={`spanel-${k}`} onclick={() => (tab = k)}>{t(`panel.settings.tab.${k}` as Key)}</button>
      {/each}
    </div>

    <div class="stack" role="tabpanel" id="spanel-connect" aria-labelledby="stab-connect" hidden={tab !== 'connect'}>
      <section class="t">
        <div class="th">
          <h2>{t('panel.settings.connect')}</h2>
          <span class="sub">{t('panel.settings.connectSub')}</span>
        </div>
        <div class="tb-b stack">
          <pre class="code" data-testid="install-command">{command}</pre>
          <div><button class="btn s" type="button" onclick={copy}><Icon name="copy" />{t('panel.tokens.copy')}</button></div>
          <p class="faint sm">{t('panel.addNode.note')}</p>
        </div>
      </section>
    </div>

    <div class="stack" role="tabpanel" id="spanel-proxy" aria-labelledby="stab-proxy" hidden={tab !== 'proxy'}>
      <SettingsForm {store} form={FORMS.px} title={t('panel.settings.proxyOnNodes')} sub={t('panel.settings.proxyOnNodesSub')}>
        {#snippet extra()}
          <div class="calm info"><Icon name="info" /><span>{t('panel.settings.agentNote')}</span></div>
        {/snippet}
      </SettingsForm>
      <section class="t" data-key="proxy_users">
        <div class="th">
          <h2>{t('panel.settings.users')}</h2>
          <span class="sub">{t('panel.settings.usersSub')}</span>
        </div>
        {#if users.length === 0}
          <div class="tb-b"><p class="muted">{t('panel.settings.usersEmpty')}</p></div>
        {:else}
          <div class="tw">
            <table class="tb list">
              <thead>
                <tr>
                  <th>{t('panel.settings.userName')}</th>
                  <th>{t('panel.settings.userSecret')}</th>
                  <th><span class="sr-only">{t('panel.nodes.actions')}</span></th>
                </tr>
              </thead>
              <tbody>
                {#each users as u (u.name)}
                  <tr>
                    <td class="full nm" data-l={t('panel.settings.userName')}>{u.name}</td>
                    <td class="mono sm" data-l={t('panel.settings.userSecret')}>{masked(u.secret_hex)}</td>
                    <td class="n"><button class="btn s" type="button" onclick={() => (renewing = u.name)}>{t('panel.settings.renew')}</button></td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        {/if}
      </section>
    </div>

    <div class="stack" role="tabpanel" id="spanel-pool" aria-labelledby="stab-pool" hidden={tab !== 'pool'}>
      <SettingsForm {store} form={FORMS.pl} title={t('panel.settings.masters')} sub={t('panel.settings.mastersSub')} />
      <SettingsForm {store} form={FORMS.dn} title={t('panel.settings.dns')} sub={t('panel.settings.dnsSub')}>
        {#snippet extra()}
          <div class="fg">{@render secretRow('cloudflare_api_token')}</div>
        {/snippet}
      </SettingsForm>
    </div>

    <div class="stack" role="tabpanel" id="spanel-checks" aria-labelledby="stab-checks" hidden={tab !== 'checks'}>
      <SettingsForm {store} form={FORMS.h0} title={t('panel.settings.h0')} sub={t('panel.settings.h0Sub')} />
      <SettingsForm {store} form={FORMS.h1} title={t('panel.settings.h1')} sub={t('panel.settings.h1Sub')} />
      <SettingsForm {store} form={FORMS.h2} title={t('panel.settings.h2')} sub={t('panel.settings.h2Sub')} />
      <SettingsForm {store} form={FORMS.gp} title={t('panel.settings.gp')} sub={t('panel.settings.gpSub')}>
        {#snippet extra()}
          <div class="fg">
            {@render secretRow('globalping_token')}
            {@render secretRow('redflag_api_key')}
          </div>
        {/snippet}
      </SettingsForm>
    </div>

    <div class="stack" role="tabpanel" id="spanel-sys" aria-labelledby="stab-sys" hidden={tab !== 'sys'}>
      <SettingsForm {store} form={FORMS.ro} title={t('panel.settings.ro')} sub={t('panel.settings.roSub')} />
    </div>
  {/if}
</div>

<Modal
  bind:open={() => replacing !== null, (v) => { if (!v) { replacing = null; secretValue = ''; } }}
  title={replacing ? t('settings.secret.title', { label: secretLabel(replacing) }) : ''}
>
  <div class="f">
    <label for="secret-new">{t('settings.secret.value')}</label>
    <input class="inp secret-in" id="secret-new" type="password" autocomplete="off" autocapitalize="off" spellcheck="false" bind:value={secretValue} />
    <div class="h">{t('settings.secret.hint')}</div>
  </div>
  {#snippet footer()}
    <button class="btn" type="button" onclick={() => { replacing = null; secretValue = ''; }}>{t('common.cancel')}</button>
    <button class="btn pri" type="button" disabled={secretValue === ''} onclick={saveSecret}>{t('common.save')}</button>
  {/snippet}
</Modal>

<Confirm
  bind:open={() => renewing !== null, (v) => { if (!v) renewing = null; }}
  title={t('panel.settings.renewTitle', { name: renewing ?? '' })}
  text={t('panel.settings.renewText')}
  keep={t('panel.srmd.releaseKeep')}
  confirm={t('panel.settings.renew')}
  {busy}
  onconfirm={renew}
/>
