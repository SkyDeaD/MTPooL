import { describe, expect, it } from 'vitest';
import { checkRows, formatAvailability, groupNodes, historySlots, panelBans, poolCounts, whyText } from '../src/lib/model';
import type { BanRecord, PublicNode } from '../src/lib/types';
import history from './fixtures/node-history.json';
import nodes from './fixtures/nodes.json';
import { FIXTURE_NOW } from './helpers';

const list = nodes as PublicNode[];

describe('model', () => {
  it('the_panel_lists_all_bans_in_force_and_fills_up_with_the_latest', () => {
    const ban = (id: number, extra: Partial<BanRecord> = {}): BanRecord => ({
      id,
      ip: '203.x.x.91',
      node_id: `n-${id}`,
      at: '2026-11-04T09:00:00Z',
      votes: [],
      reverify: false,
      lifted_by_admin: false,
      ...extra,
    });
    const lifted = { lifted_at: '2026-11-04T10:00:00Z' };
    const ids = (rows: BanRecord[]) => rows.map((b) => b.id);
    const log = [ban(9, lifted), ban(8, { reverify: true }), ban(7, lifted), ban(6), ban(5, lifted)];

    expect(ids(panelBans([], log, 2))).toEqual([9, 7]);
    // A ban in force is not shown twice, and it takes a place of the latest.
    expect(ids(panelBans([ban(6)], log, 3))).toEqual([9, 7, 6]);
    // More in force than places: all of them stay, an old one too.
    expect(ids(panelBans([ban(3), ban(2), ban(1)], log, 2))).toEqual([3, 2, 1]);
    expect(ids(panelBans([], [], 5))).toEqual([]);
  });

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

  it('lists_every_check_by_name_with_how_it_went', () => {
    const states = (checks: Parameters<typeof checkRows>[0]) => checkRows(checks).map((r) => `${r.name}:${r.state}`);
    expect(states({ tcp: true, metrics: true, globalping: 'ok' })).toEqual(['tcp:ok', 'metrics:ok', 'globalping:ok']);
    expect(states({ tcp: false, metrics: false, globalping: 'failed' })).toEqual(['tcp:bad', 'metrics:bad', 'globalping:bad']);
    expect(states({ tcp: true, metrics: true, globalping: 'no_data' })).toEqual(['tcp:ok', 'metrics:ok', 'globalping:none']);
  });

  it('puts_quarantine_reasons_in_words_and_keeps_a_code_it_does_not_know', () => {
    expect(whyText(['report_stale'])).toBe('нет свежего замера Globalping');
    expect(whyText(['pool_user_taken'])).toBe('на ноде уже есть пользователь с именем из настроек пула и другим секретом');
    expect(whyText(['tcp', 'something_new'])).toBe('порт не отвечает снаружи, something_new');
    expect(whyText([])).toBe('');
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
