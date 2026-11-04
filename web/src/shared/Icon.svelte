<script lang="ts">
  import { ICONS, type IconName } from '../lib/icons';

  let { name, lg = false, cls = '' }: { name: IconName; lg?: boolean; cls?: string } = $props();
</script>

<svg class={['i', lg && 'lg', cls]} viewBox="0 0 24 24" aria-hidden="true">{@html ICONS[name]}</svg>
