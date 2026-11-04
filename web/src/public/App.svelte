<script lang="ts">
  import { t } from '../lib/i18n';
</script>

<main class="page gut">
  <header class="ph">
    <h1>{t('public.stub.title')}</h1>
    <span class="live"><span class="dot"></span>{t('public.stub.status')}</span>
    <p class="sub">{t('app.tagline')}</p>
  </header>
  <section class="t pad">
    <p class="muted">{t('public.stub.text')}</p>
  </section>
</main>

<style>
  .page {
    max-width: var(--w-public);
    margin: 0 auto;
    padding-top: 24px;
    padding-bottom: 24px;
  }
</style>
