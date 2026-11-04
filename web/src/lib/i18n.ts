import { messages, plurals, type MessageKey, type PluralKey } from '../strings.ru';

/** The text for `key`; `{name}` placeholders are filled from `params`. */
export function t(key: MessageKey, params?: Record<string, string | number>): string {
  const text: string = messages[key];
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (whole, name: string) =>
    name in params ? String(params[name]) : whole,
  );
}

/** Picks the Russian form for `n` from [one, few, many]. */
export function plural(n: number, forms: readonly [string, string, string]): string {
  const whole = Math.abs(Math.trunc(n));
  const last = whole % 10;
  const lastTwo = whole % 100;
  if (last === 1 && lastTwo !== 11) return forms[0];
  if (last >= 2 && last <= 4 && (lastTwo < 12 || lastTwo > 14)) return forms[1];
  return forms[2];
}

/** The form of a counted word from the strings file, such as `unit.day`. */
export function tp(key: PluralKey, n: number): string {
  return plural(n, plurals[key]);
}
