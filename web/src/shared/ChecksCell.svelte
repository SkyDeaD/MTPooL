<script lang="ts">
  import { t } from '../lib/i18n';
  import { checksProblems } from '../lib/model';
  import type { NodeChecks } from '../lib/types';

  let { checks }: { checks: NodeChecks } = $props();

  const problems = $derived(checksProblems(checks));
  const failed = $derived(problems.filter((p) => p !== 'globalping_nodata'));
  const names = $derived(failed.map((p) => t(`check.${p}`)).join(', '));
</script>

{#if problems.length === 0}
  <span class="muted" title={t('check.allTitle')}>{t('check.all')}</span>
{:else if failed.length > 0}
  <span class="st warn">{t('check.failed', { list: names })}</span>
{:else}
  <span class="muted">{t('check.noData')}</span>
{/if}
