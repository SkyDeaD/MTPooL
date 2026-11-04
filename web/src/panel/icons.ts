// Icons only the panel draws. They live apart from `lib/icons.ts` so the public pages do not carry them.
import { ICONS } from '../lib/icons';

export const PANEL_ICONS = {
  grid: '<rect x="4" y="4" width="6.500" height="6.500" rx="1.500"/><rect x="13.500" y="4" width="6.500" height="6.500" rx="1.500"/><rect x="4" y="13.500" width="6.500" height="6.500" rx="1.500"/><rect x="13.500" y="13.500" width="6.500" height="6.500" rx="1.500"/>',
  server: '<rect x="3.500" y="4.500" width="17" height="6" rx="1.500"/><rect x="3.500" y="13.500" width="17" height="6" rx="1.500"/><path d="M7 7.500h.01M7 16.500h.01"/>',
  globe: '<circle cx="12" cy="12" r="8.500"/><path d="M3.500 12h17M12 3.500c2.500 2.500 3.500 5.300 3.500 8.500s-1 6-3.500 8.500c-2.500-2.500-3.500-5.300-3.500-8.500s1-6 3.500-8.500z"/>',
  layers: '<path d="m12 4 8.500 4.500L12 13 3.500 8.500z"/><path d="m3.500 12.500 8.500 4.500 8.500-4.500M3.500 16.500 12 21l8.500-4.500"/>',
  list: '<path d="M8.500 7h11M8.500 12h11M8.500 17h11M4.500 7h.01M4.500 12h.01M4.500 17h.01"/>',
  sliders: '<path d="M4 7h9M17 7h3M4 17h3M11 17h9"/><circle cx="15" cy="7" r="2"/><circle cx="9" cy="17" r="2"/>',
  users: '<circle cx="9" cy="8.500" r="3.200"/><path d="M3.500 19c.4-3.300 2.600-5 5.500-5s5.100 1.700 5.500 5M16 5.600a3.200 3.200 0 0 1 0 5.800M17.500 14.300c1.800.5 2.800 2 3 4.700"/>',
  up: '<path d="M12 19.500v-14M6 11.500l6-6 6 6"/>',
  out: '<path d="M10 4.500H6A1.500 1.500 0 0 0 4.500 6v12A1.500 1.500 0 0 0 6 19.500h4M15 8l4 4-4 4M19 12H9.500"/>',
  search: '<circle cx="11" cy="11" r="6.500"/><path d="m16 16 4 4"/>',
  more: '<path d="M6 12h.01M12 12h.01M18 12h.01"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  key: '<circle cx="8.500" cy="14.500" r="3.700"/><path d="m11.200 11.800 8-8M16 7l2.500 2.500M14 9l2 2"/>',
  pause: '<path d="M9 6v12M15 6v12"/>',
  play: '<path d="M8 5.500v13l10.500-6.500z"/>',
  eye: '<path d="M3 12s3.200-6 9-6 9 6 9 6-3.200 6-9 6-9-6-9-6z"/><circle cx="12" cy="12" r="2.500"/>',
  elbow: '<path d="M18 5v6.500a3.500 3.500 0 0 1-3.500 3.500H6"/><path d="m9.500 11.500-3.500 3.500 3.500 3.500"/>',
  info: '<circle cx="12" cy="12" r="8.5"/><path d="M12 11v5M12 8h.01"/>',
  shield: '<path d="M12 3.500 5 6.500v5c0 4.300 2.800 7.500 7 9 4.200-1.500 7-4.700 7-9v-5z"/>',
} as const;

export const ALL_ICONS = { ...ICONS, ...PANEL_ICONS } as const;
export type IconName = keyof typeof ALL_ICONS;
