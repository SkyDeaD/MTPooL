<script lang="ts">
  import { Select } from 'bits-ui';
  import Icon from '../../shared/Icon.svelte';

  // A list to choose one from, on Bits UI: keyboard, typeahead and the screen reader come with it.
  let {
    value = $bindable(''),
    items,
    label,
  }: { value?: string; items: { value: string; label: string }[]; label: string } = $props();

  const shown = $derived(items.find((i) => i.value === value)?.label ?? '');
</script>

<Select.Root type="single" bind:value {items}>
  <Select.Trigger class="sel-trigger" aria-label={label}>
    <span>{shown}</span>
    <Icon name="chev" />
  </Select.Trigger>
  <Select.Portal>
    <Select.Content class="sel-list" sideOffset={4}>
      <Select.Viewport>
        {#each items as item (item.value)}
          <Select.Item class="opt" value={item.value} label={item.label}>{item.label}</Select.Item>
        {/each}
      </Select.Viewport>
    </Select.Content>
  </Select.Portal>
</Select.Root>
