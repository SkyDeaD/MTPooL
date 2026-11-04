// Light and dark theme. The choice lives in `data-theme` on <html> and in localStorage; without
// a choice the system's colour scheme decides (tokens.css). Storage can be blocked, so every
// access is guarded.
const KEY = 'mtpool-theme';

export type Theme = 'light' | 'dark';

export function currentTheme(): Theme {
  const set = document.documentElement.dataset.theme;
  if (set === 'light' || set === 'dark') return set;
  return window.matchMedia?.('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

/** Applies a saved choice, if any. Called before the page is attached. */
export function applySavedTheme(): void {
  try {
    const saved = localStorage.getItem(KEY);
    if (saved === 'light' || saved === 'dark') document.documentElement.dataset.theme = saved;
  } catch {
    // Storage is not available: the system scheme stays.
  }
}

export function toggleTheme(): Theme {
  const next: Theme = currentTheme() === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  try {
    localStorage.setItem(KEY, next);
  } catch {
    // The choice lasts until the page is closed.
  }
  return next;
}
