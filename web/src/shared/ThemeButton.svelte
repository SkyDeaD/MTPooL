<script lang="ts">
  import { t } from '../lib/i18n';
  import { currentTheme, toggleTheme, type Theme } from '../lib/theme';
  import Icon from './Icon.svelte';

  // The page is prerendered without a theme; the icon is set once it is in the browser.
  let theme = $state<Theme>('light');
  $effect(() => {
    theme = currentTheme();
  });
</script>

<button class="btn s ic" type="button" aria-label={t('shell.theme')} onclick={() => (theme = toggleTheme())}>
  <Icon name={theme === 'dark' ? 'sun' : 'moon'} />
</button>
