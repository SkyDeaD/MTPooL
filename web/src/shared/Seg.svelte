<script lang="ts" generics="V extends string">
  // A row of buttons where one is chosen, such as a range or a metric.
  let {
    items,
    value,
    onchange,
    label,
  }: {
    items: readonly { value: V; label: string }[];
    value: V;
    onchange: (value: V) => void;
    label: string;
  } = $props();
</script>

<div class="seg" role="group" aria-label={label}>
  {#each items as item (item.value)}
    <button type="button" aria-pressed={item.value === value} onclick={() => onchange(item.value)}>
      {item.label}
    </button>
  {/each}
</div>
