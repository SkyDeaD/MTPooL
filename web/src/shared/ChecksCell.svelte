<script lang="ts">
  import { t } from '../lib/i18n';
  import { checkRows } from '../lib/model';
  import type { NodeChecks } from '../lib/types';
  import Icon from './Icon.svelte';

  // Each check by name, so nobody has to guess what passed: the mark and the colour say how it
  // went, the title says what it means.
  let { checks }: { checks: NodeChecks } = $props();

  const rows = $derived(checkRows(checks));
  const ICON = { ok: 'check', bad: 'stop', none: 'alert' } as const;
</script>

<ul class="chks" aria-label={t('check.label')}>
  {#each rows as row (row.name)}
    <li class={row.state} title={t(row.title)}>
      <Icon name={ICON[row.state]} />
      {t(`check.name.${row.name}`)}<span class="sr-only">: {t(`check.state.${row.state}`)}</span>
    </li>
  {/each}
</ul>
