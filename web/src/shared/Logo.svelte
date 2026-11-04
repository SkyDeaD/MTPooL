<script lang="ts">
  import { t } from '../lib/i18n';

  let { href }: { href: string } = $props();
</script>

<a class="logo" {href}>
  <svg viewBox="0 0 24 24" aria-hidden="true"
    ><rect width="24" height="24" rx="6" fill="var(--acc)" /><g
      fill="none"
      stroke="var(--acc-fg)"
      stroke-width="1.7"
      stroke-linecap="round"><path d="M12 12 6.8 7.3M12 12l5.2-4.7M12 12v6" /></g
    ><g fill="var(--acc-fg)"
      ><circle cx="12" cy="12" r="2.4" /><circle cx="6.5" cy="7" r="1.7" /><circle
        cx="17.5"
        cy="7"
        r="1.7"
      /><circle cx="12" cy="18.2" r="1.7" /></g
    ></svg
  >{t('app.name')}
</a>
