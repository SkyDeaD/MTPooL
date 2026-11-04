<script lang="ts">
  import { Popover } from 'bits-ui';
  import { t } from './i18n';
  import Icon from './ui/Icon.svelte';
  import { hrefOf } from './route.svelte';

  // The actions of one node row. Retiring is the only one that does not run at once.
  let {
    id,
    onrecheck,
    onretire,
  }: { id: string; onrecheck: () => void; onretire: () => void } = $props();

  let open = $state(false);
  const run = (fn: () => void) => () => {
    open = false;
    fn();
  };
</script>

<Popover.Root bind:open>
  <Popover.Trigger class="btn s ic txt" aria-label={t('panel.nodes.actions')}>
    <Icon name="more" />
  </Popover.Trigger>
  <Popover.Portal>
    <Popover.Content class="pop" align="end" sideOffset={4}>
      <a href={hrefOf('node', id)} style="display:flex;gap:8px;padding:8px 10px;color:var(--fg)" onclick={() => (open = false)}>
        <Icon name="chev" />{t('panel.nodes.open')}
      </a>
      <button type="button" onclick={run(onrecheck)}><Icon name="ref" />{t('panel.nodes.recheck')}</button>
      <button type="button" class="dng" onclick={run(onretire)}><Icon name="stop" />{t('panel.nodes.retire')}</button>
    </Popover.Content>
  </Popover.Portal>
</Popover.Root>
