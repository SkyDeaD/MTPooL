import { byteUnits } from '../strings.ru';
import { tp } from './i18n';

const NO_BREAK_SPACE = ' ';

const DURATION_UNITS = [
  ['unit.day', 86400],
  ['unit.hour', 3600],
  ['unit.minute', 60],
  ['unit.second', 1],
] as const;

/**
 * A span of seconds in words, the two largest non-zero units at most: "2 дня 3 часа",
 * "1 минута 1 секунда". Fractions are dropped; a bad or negative input reads as zero.
 */
export function formatDuration(seconds: number): string {
  let rest = Number.isFinite(seconds) && seconds > 0 ? Math.floor(seconds) : 0;
  const parts: string[] = [];
  for (const [key, size] of DURATION_UNITS) {
    const count = Math.floor(rest / size);
    rest -= count * size;
    if (count > 0 && parts.length < 2) parts.push(`${count} ${tp(key, count)}`);
  }
  return parts.length > 0 ? parts.join(' ') : `0 ${tp('unit.second', 0)}`;
}

/**
 * A size in bytes with a Russian unit and a decimal comma: "1,5 КБ". Steps are 1024. One
 * decimal below 10, none above. A no-break space keeps the number with its unit.
 */
export function formatBytes(bytes: number): string {
  let value = Number.isFinite(bytes) && bytes > 0 ? bytes : 0;
  let unit = 0;
  while (value >= 1024 && unit < byteUnits.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit > 0 && value < 10 ? 1 : 0;
  const text = digits ? trimZero(value.toFixed(digits)) : String(Math.round(value));
  return `${text.replace('.', ',')}${NO_BREAK_SPACE}${byteUnits[unit]}`;
}

function trimZero(text: string): string {
  return text.endsWith('.0') ? text.slice(0, -2) : text;
}
