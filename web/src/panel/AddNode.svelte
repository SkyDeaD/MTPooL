<script lang="ts">
  import { toast } from '../lib/toast.svelte';
  import { t } from './i18n';
  import { installCommand, copyText } from './install';
  import { hrefOf } from './route.svelte';
  import Icon from './ui/Icon.svelte';
  import Modal from './ui/Modal.svelte';

  // «Подключить ноду»: the command to run on the new server.
  let { open = $bindable(false) }: { open?: boolean } = $props();
  const command = $derived(installCommand());

  async function copy(): Promise<void> {
    toast((await copyText(command)) ? t('panel.tokens.copied') : t('panel.tokens.copyFailed'));
  }
</script>

<Modal bind:open title={t('panel.addNode.title')}>
  <p class="muted">{t('panel.addNode.text')}</p>
  <pre class="code" data-testid="install-command">{command}</pre>
  <div><button class="btn s" type="button" onclick={copy}><Icon name="copy" />{t('panel.tokens.copy')}</button></div>
  <p class="faint sm">{t('panel.addNode.note')}</p>
  {#snippet footer()}
    <a class="btn" href={hrefOf('owners')} onclick={() => (open = false)}>{t('panel.addNode.issue')}</a>
    <button class="btn pri" type="button" onclick={() => (open = false)}>{t('panel.addNode.done')}</button>
  {/snippet}
</Modal>
