import { describe, expect, it } from 'vitest';
import { formatAvailability, groupNodes, historySlots, poolCounts, checksProblems } from '../src/lib/model';
import type { PublicNode } from '../src/lib/types';
import history from './fixtures/node-history.json';
import nodes from './fixtures/nodes.json';
import { FIXTURE_NOW } from './helpers';

const list = nodes as PublicNode[];

describe('model', () => {
  it('groups_nodes_and_leaves_out_the_rest', () => {
    const extra = ['pending', 'dead', 'pruned'].map((state) => ({ ...list[0], id: `x-${state}`, is_master: false, state }));
    const g = groupNodes([...list, ...(extra as PublicNode[])]);
    expect(g.masters.map((n) => n.id)).toEqual(['alpha-k3m2q', 'beta-r8t4w', 'gamma-x2n9d']);
    expect(g.queue.map((n) => n.id)).toEqual(['epsilon-h4j6s', 'zeta-w9b3y', 'eta-c5f8l']);
    expect(g.quarantine.map((n) => n.id)).toEqual(['theta-m1d7z']);
  });

  it('counts_working_nodes_and_mean_availability', () => {
    const c = poolCounts(list);
    expect(c.shown).toBe(7);
    expect(c.working).toBe(6);
    expect(c.availabilityBp).toBeCloseTo(9789.17, 1);
    expect(poolCounts([]).availabilityBp).toBeNull();
  });

  it('formats_availability_with_a_dash_for_no_data', () => {
    expect(formatAvailability(9987)).toBe('99,87 %');
    expect(formatAvailability(10000)).toBe('100 %');
    expect(formatAvailability(null)).toBe('—');
    expect(formatAvailability(undefined)).toBe('—');
  });

  it('names_the_failed_checks', () => {
    expect(checksProblems({ tcp: true, metrics: true, globalping: 'ok' })).toEqual([]);
    expect(checksProblems({ tcp: false, metrics: false, globalping: 'failed' })).toEqual(['tcp', 'globalping', 'metrics']);
    expect(checksProblems({ tcp: true, metrics: true, globalping: 'no_data' })).toEqual(['globalping_nodata']);
  });

  it('history_has_96_slots_and_a_missing_bucket_is_no_data_not_zero', () => {
    const slots = historySlots(history, FIXTURE_NOW);
    expect(slots).toHaveLength(96);
    expect(slots.filter((s) => s.state === 'none')).toHaveLength(5 + 1);
    expect(slots.filter((s) => s.state === 'bad')).toHaveLength(1);
    expect(slots.filter((s) => s.state === 'warn')).toHaveLength(1);
    // Oldest first, and the newest slot is the current, still unsampled, bucket.
    expect(slots[95].state).toBe('none');
    expect(slots[0].clients).not.toBeNull();
    expect(slots[53].clients).toBeNull();
  });
});
