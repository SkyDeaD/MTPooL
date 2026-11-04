<script lang="ts">
  import { t } from '../lib/i18n';
  import { act } from './act';
  import { usePanel } from './context.svelte';
  import Confirm from './ui/Confirm.svelte';

  // Taking a node out of the pool cannot be undone, so it is asked twice.
  let { id = $bindable<string | null>(null), onretired }: { id?: string | null; onretired?: () => void } = $props();

  const panel = usePanel();
  let busy = $state(false);


  async function retire(): Promise<void> {
    if (id === null) return;
    busy = true;
    const target = id;
    const ok = await act(
      panel,
      () => panel.client.post('/api/v1/admin/nodes/{id}/retire', { params: { id: target } }),
      t('panel.retire.done', { id: target }),
    );
    busy = false;
    id = null;
    if (ok) onretired?.();
  }
</script>

<Confirm
  bind:open={() => id !== null, (v) => { if (!v) id = null; }}  title={t('panel.retire.title', { id: id ?? '' })}
  text={t('panel.retire.text')}
  keep={t('panel.retire.keep')}
  confirm={t('panel.retire.confirm')}
  {busy}
  onconfirm={retire}
/>
