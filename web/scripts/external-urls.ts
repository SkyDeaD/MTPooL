// Finds addresses of other hosts in text. The interface must not fetch anything from outside:
// fonts, scripts and styles are built into the binary. Only the namespace names the browser
// needs and the project's own documentation links may appear.

const ALLOWED_PREFIXES = [
  // XML namespace names, not fetched by anything: Svelte's runtime and inline SVG carry them.
  'http://www.w3.org/1999/xhtml',
  'http://www.w3.org/2000/svg',
  'http://www.w3.org/1998/Math/MathML',
  'http://www.w3.org/1999/xlink',
  // Svelte's runtime puts this address into the text of its error messages. Nothing fetches it.
  'https://svelte.dev/e/',
  // Documentation links the pages may show.
  'https://github.com/SkyDeaD/MTPooL',
  'https://core.telegram.org/',
];

// A scheme with `//`, or `//` that starts a value: after a quote, `(`, `=` or whitespace.
// A comment slash in code is left alone by requiring a host name right after.
const URL_PATTERN = /(?:https?:)?\/\/[a-z0-9-]+(?:\.[a-z0-9-]+)+[^\s"'`)<>\\]*/gi;

export function findExternalUrls(text: string, allowed: readonly string[] = ALLOWED_PREFIXES): string[] {
  const found: string[] = [];
  for (const match of text.matchAll(URL_PATTERN)) {
    const url = match[0];
    const before = match.index > 0 ? text[match.index - 1] : ' ';
    // `x//y` and `/a//b` are paths, not addresses.
    if (url.startsWith('//') && !/[\s"'`(=]/.test(before)) continue;
    if (!url.startsWith('//') && /[A-Za-z0-9]/.test(before)) continue;
    if (allowed.some((prefix) => url.startsWith(prefix))) continue;
    found.push(url);
  }
  return found;
}
