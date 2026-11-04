<script lang="ts">
  import { toasts } from '../lib/toast.svelte';
  import Icon from './Icon.svelte';
</script>

<div class="toasts" role="status" aria-live="polite">
  {#each toasts.list as item (item.id)}
    <div class="toast"><Icon name="check" />{item.text}</div>
  {/each}
</div>
