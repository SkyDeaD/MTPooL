<script lang="ts">
  import { ALL_ICONS, type IconName } from '../icons';

  let { name, lg = false, cls = '' }: { name: IconName; lg?: boolean; cls?: string } = $props();
</script>

<svg class={['i', lg && 'lg', cls]} viewBox="0 0 24 24" aria-hidden="true">{@html ALL_ICONS[name]}</svg>
