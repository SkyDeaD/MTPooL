// Icon shapes on a 24 by 24 grid, drawn with the stroke of the surrounding text.
// Shared by the public pages and the panel; Icon.svelte draws one.
export const ICONS = {
  minus: '<circle cx="12" cy="12" r="8.5"/><path d="M8.5 12h7"/>',
  check: '<circle cx="12" cy="12" r="8.5"/><path d="m8.5 12.3 2.4 2.4 4.6-5"/>',
  alert: '<path d="M12 4 2.8 19.500h18.400z"/><path d="M12 10v4.500M12 17h.01"/>',
  copy: '<rect x="8.5" y="8.5" width="11.5" height="11.5" rx="2"/><path d="M5.5 15.500H5a1.5 1.5 0 0 1-1.5-1.500V5A1.5 1.5 0 0 1 5 3.500h9A1.5 1.5 0 0 1 15.5 5v.5"/>',
  qr: '<rect x="4" y="4" width="6.5" height="6.5" rx="1"/><rect x="13.5" y="4" width="6.5" height="6.5" rx="1"/><rect x="4" y="13.5" width="6.5" height="6.5" rx="1"/><path d="M14 14h2.500v2.500M20 14v6h-3M14 20h.01"/>',
  send: '<path d="M20.5 4 3.5 11l6.2 2.500L12 20z"/><path d="m9.7 13.5 4.3-3.7"/>',
  sun: '<circle cx="12" cy="12" r="3.7"/><path d="M12 3.500v2M12 18.500v2M3.5 12h2M18.5 12h2M6 6l1.4 1.400M16.6 16.6 18 18M6 18l1.4-1.400M16.6 7.4 18 6"/>',
  moon: '<path d="M19.5 14.200A8 8 0 0 1 9.8 4.500a8 8 0 1 0 9.7 9.700z"/>',
  back: '<path d="M14.5 6.5 9 12l5.5 5.5"/>',
  chev: '<path d="m9.5 6.5 5.5 5.5-5.5 5.5"/>',
  clock: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7.500V12l3 2"/>',
  stop: '<circle cx="12" cy="12" r="8.5"/><path d="m6.2 6.2 11.6 11.6"/>',
  x: '<path d="M6 6l12 12M18 6 6 18"/>',
  ref: '<path d="M19.5 8.500A8 8 0 0 0 5 7.500L3.5 9.500M4.5 15.500A8 8 0 0 0 19 16.500l1.5-2M3.5 5v4.500H8M20.5 19v-4.500H16"/>',
} as const;

export type IconName = keyof typeof ICONS;
