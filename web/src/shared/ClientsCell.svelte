<script lang="ts">
  /** The number of clients with a bar relative to the largest one; a dash when unknown. */
  let { clients, max }: { clients: number | null | undefined; max: number } = $props();

  const width = $derived(
    typeof clients === 'number' && max > 0 ? Math.round((100 * clients) / max) : 0,
  );
</script>

{#if typeof clients === 'number'}
  {clients}<span class="bar gr"><i style="width:{width}%"></i></span>
{:else}
  <span class="faint">—</span>
{/if}
