// The panel's `t`: the panel texts and the shared ones, one lookup.
import { plural, tp } from '../lib/i18n';
import { messages as shared } from '../strings.ru';
import { panelMessages } from '../strings.panel.ru';

export const messages = { ...shared, ...panelMessages } as const;
export type Key = keyof typeof messages;

/** The text for `key`; `{name}` placeholders are filled from `params`. */
export function t(key: Key, params?: Record<string, string | number>): string {
  const text: string = messages[key];
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (whole, name: string) => (name in params ? String(params[name]) : whole));
}

export { plural, tp };
