<script lang="ts">
  import Modal from './Modal.svelte';

  // Asks before something that cannot be taken back. `keep` is the safe answer and comes first.
  let {
    open = $bindable(false),
    title,
    text,
    keep,
    confirm,
    busy = false,
    onconfirm,
  }: {
    open?: boolean;
    title: string;
    text: string;
    keep: string;
    confirm: string;
    busy?: boolean;
    onconfirm: () => void;
  } = $props();
</script>

<Modal bind:open {title}>
  <p class="muted">{text}</p>
  {#snippet footer()}
    <button class="btn" type="button" onclick={() => (open = false)}>{keep}</button>
    <button class="btn dng" type="button" disabled={busy} onclick={onconfirm}>{confirm}</button>
  {/snippet}
</Modal>
