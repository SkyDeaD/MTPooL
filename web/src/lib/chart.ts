// The small maths of the hand-drawn charts.

/** One decimal, as a number: keeps path data short. */
export const round1 = (n: number): number => Math.round(n * 10) / 10;

/**
 * A smooth path through the points that never overshoots them (a monotone cubic), so the line
 * does not dip below a flat stretch or climb past a peak.
 */
export function monotonePath(p: readonly (readonly [number, number])[]): string {
  const n = p.length;
  if (n === 0) return '';
  if (n === 1) return `M${round1(p[0][0])} ${round1(p[0][1])}`;
  const dx: number[] = [];
  const m: number[] = [];
  const tg: number[] = [];
  for (let i = 0; i < n - 1; i++) {
    dx[i] = p[i + 1][0] - p[i][0] || 1;
    m[i] = (p[i + 1][1] - p[i][1]) / dx[i];
  }
  tg[0] = m[0];
  tg[n - 1] = m[n - 2];
  for (let i = 1; i < n - 1; i++) {
    tg[i] = m[i - 1] * m[i] <= 0 ? 0 : (2 * m[i - 1] * m[i]) / (m[i - 1] + m[i]);
  }
  let d = `M${round1(p[0][0])} ${round1(p[0][1])}`;
  for (let i = 0; i < n - 1; i++) {
    const h = dx[i] / 3;
    d += `C${round1(p[i][0] + h)} ${round1(p[i][1] + tg[i] * h)} ${round1(p[i + 1][0] - h)} ${round1(p[i + 1][1] - tg[i + 1] * h)} ${round1(p[i + 1][0])} ${round1(p[i + 1][1])}`;
  }
  return d;
}

/** A round step for an axis: 1, 2, 2.5, 5 or 10 times a power of ten. */
// Ticks for a count of things: whole numbers only, so a small scale does not label 0.5 as 1.
export function countTicks(lo: number, hi: number): number[] {
  const step = Math.max(1, Math.ceil(niceStep((hi - lo) / 2.5)));
  const out: number[] = [];
  for (let v = Math.ceil(lo / step) * step; v <= hi + 1e-6 && out.length < 7; v += step) out.push(v);
  return out;
}

export function niceStep(x: number): number {
  const e = Math.pow(10, Math.floor(Math.log10(x || 1)));
  const f = x / e;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10) * e;
}
