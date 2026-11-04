<script lang="ts">
  import { Dialog } from 'bits-ui';
  import type { Snippet } from 'svelte';

  // A dialog on Bits UI: focus stays inside, Escape closes, the page behind is inert.
  let {
    open = $bindable(false),
    title,
    children,
    footer,
    onclose,
  }: {
    open?: boolean;
    title: string;
    children: Snippet;
    footer?: Snippet;
    onclose?: () => void;
  } = $props();
</script>

<Dialog.Root bind:open onOpenChange={(v) => !v && onclose?.()}>
  <Dialog.Portal>
    <Dialog.Overlay class="dlg-ov" />
    <Dialog.Content class="dlg">
      <Dialog.Title><h2>{title}</h2></Dialog.Title>
      <div class="body">{@render children()}</div>
      {#if footer}<div class="ff">{@render footer()}</div>{/if}
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
