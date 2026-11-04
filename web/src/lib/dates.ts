// Numbers and moments as the pages show them: Russian formats, Moscow time.
const ZONE = 'Europe/Moscow';
const numberFormat = new Intl.NumberFormat('ru-RU');
const hourFormat = new Intl.DateTimeFormat('ru-RU', { timeZone: ZONE, hour: '2-digit', minute: '2-digit' });
const secondFormat = new Intl.DateTimeFormat('ru-RU', {
  timeZone: ZONE,
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
});
const dayFormat = new Intl.DateTimeFormat('ru-RU', { timeZone: ZONE, day: 'numeric', month: 'short' });

/** A whole number with grouping: "1 234". */
export function formatNumber(n: number): string {
  return numberFormat.format(Math.round(n));
}

/** "14:05" */
export function formatHour(ms: number): string {
  return hourFormat.format(ms);
}

/** "14:05:30" */
export function formatSecond(ms: number): string {
  return secondFormat.format(ms);
}

/** "4 нояб." */
export function formatDay(ms: number): string {
  return dayFormat.format(ms);
}

/** "4 нояб., 14:05" */
export function formatWhen(ms: number): string {
  return `${formatDay(ms)}, ${formatHour(ms)}`;
}

/** "3 д 4 ч", "4 ч 5 мин", "5 мин": the short span the ban figures use. */
export function formatSpan(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d} д ${h} ч`;
  if (h > 0) return `${h} ч ${m} мин`;
  return `${m} мин`;
}
