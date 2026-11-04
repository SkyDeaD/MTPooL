<script lang="ts">
  import { t } from '../lib/i18n';
  import type { PublicNode } from '../lib/types';
  import Icon from './Icon.svelte';

  let { state }: { state: PublicNode['state'] } = $props();
</script>

{#if state === 'quarantine'}
  <span class="st warn"><Icon name="alert" />{t('node.state.quarantine')}</span>
{:else if state === 'banned'}
  <span class="st bad"><Icon name="stop" />{t('node.state.banned')}</span>
{:else}
  <span class="st ok"><Icon name="check" />{t('node.state.healthy')}</span>
{/if}
