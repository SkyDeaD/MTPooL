// The current time as a reactive value, for countdowns and "updated N seconds ago" texts.
// Call it while a component is being created. Nothing ticks on the server or in a hidden tab.

export function useNow(intervalMs = 1000): { readonly ms: number } {
  let ms = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => {
      if (!document.hidden) ms = Date.now();
    }, intervalMs);
    const onVisible = () => {
      if (!document.hidden) ms = Date.now();
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      clearInterval(timer);
      document.removeEventListener('visibilitychange', onVisible);
    };
  });
  return {
    get ms() {
      return ms;
    },
  };
}
