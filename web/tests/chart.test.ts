import { describe, expect, it } from 'vitest';
import { countTicks } from '../src/lib/chart';

describe('countTicks', () => {
  it('a_count_has_no_fractional_ticks_so_labels_never_repeat', () => {
    expect(countTicks(0, 1)).toEqual([0, 1]);
    expect(countTicks(0, 2)).toEqual([0, 1, 2]);
  });

  it('a_large_scale_gets_round_ticks', () => {
    expect(countTicks(0, 450)).toEqual([0, 200, 400]);
  });

  it('a_narrow_scale_away_from_zero_stays_on_whole_numbers', () => {
    const ticks = countTicks(440.2, 446.4);
    expect(ticks.length).toBeGreaterThan(1);
    for (const tick of ticks) expect(Number.isInteger(tick)).toBe(true);
  });
});
