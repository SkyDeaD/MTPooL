<script lang="ts">
  import { t } from '../lib/i18n';
</script>

<main class="page gut">
  <header class="ph">
    <h1>{t('panel.stub.title')}</h1>
  </header>
  <section class="t pad">
    <p class="muted">{t('panel.stub.text')}</p>
  </section>
</main>

<style>
  .page {
    max-width: var(--w-panel);
    margin: 0 auto;
    padding-top: 24px;
    padding-bottom: 24px;
  }
</style>
