<script lang="ts">
  import { t } from '../lib/i18n';

  /** The role mark; `domain` is the short name of the domain the node is master of. */
  let { domain }: { domain?: string } = $props();
</script>

<span class="mrole"
  ><span class="mtag">{t('node.role.master')}</span>{#if domain}<span class="mono">{domain}</span>{/if}</span
>
