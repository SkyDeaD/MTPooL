// RapiDoc, the page that tries requests, is a 640 KB script. It is not part of the page: it is added
// when the reader asks for it, from our own host.
export const RAPIDOC_SRC = '/rapidoc-min.js';

let loading: Promise<void> | null = null;

export function loadRapidoc(): Promise<void> {
  if (loading) return loading;
  loading = new Promise<void>((resolve, reject) => {
    const script = document.createElement('script');
    script.src = RAPIDOC_SRC;
    script.onload = () => resolve();
    script.onerror = () => {
      loading = null;
      script.remove();
      reject(new Error('rapidoc did not load'));
    };
    document.head.append(script);
  });
  return loading;
}
