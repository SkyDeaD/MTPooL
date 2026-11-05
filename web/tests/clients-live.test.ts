// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';
import { MIN_READ_GAP_MS, mergeLive, readDelay, READ_SPREAD_MS, withLiveClients, type LiveTick } from '../src/lib/clients-live';
import type { ClientsSeries, PublicDomain, PublicNode } from '../src/lib/types';

const at = (s: number) => new Date(Date.UTC(2026, 10, 5, 12, 0, s)).toISOString();
const series = (range: ClientsSeries['range'], ...seconds: number[]): ClientsSeries => ({
  range,
  step_sec: 30,
  marks: [],
  points: seconds.map((s) => ({ at: at(s), total: 10, domains: {} })),
});
const tick = (s: number, total: number): LiveTick => ({ at: at(s), total, domains: {}, nodes: {} });

describe('live clients', () => {
  it('adds what came after the last known point, in order', () => {
    const merged = mergeLive(series('10m', 0, 30), [tick(25, 1), tick(35, 2), tick(40, 3)]);
    expect(merged.points.map((p) => p.total)).toEqual([10, 10, 2, 3]);
  });

  it('adds only the newest moment to a range whose step is far longer than a tick', () => {
    const merged = mergeLive(series('24h', 0, 30), [tick(35, 2), tick(40, 3)]);
    expect(merged.points.map((p) => p.total)).toEqual([10, 10, 3]);
  });

  it('keeps the series as it is without ticks', () => {
    const base = series('10m', 0);
    expect(mergeLive(base, [])).toBe(base);
  });

  it('puts the latest numbers on the nodes and domains the tick knows', () => {
    const nodes = [{ id: 'a', clients: 1 }, { id: 'b', clients: 2 }] as PublicNode[];
    const domains = [{ domain: 'x.example', clients: 3 }] as PublicDomain[];
    const live: LiveTick = { at: at(0), total: 9, domains: { 'x.example': 9 }, nodes: { a: 7 } };
    const out = withLiveClients(nodes, domains, live);
    expect(out.nodes.map((n) => n.clients)).toEqual([7, 2]);
    expect(out.domains[0].clients).toBe(9);
    expect(withLiveClients(nodes, domains, null).nodes).toBe(nodes);
  });

  it('reads the lists soon after the first change and rarely after that', () => {
    expect(readDelay(1000, null, 0.5)).toBe(READ_SPREAD_MS / 2);
    expect(readDelay(1000, 1000, 0.5)).toBe(MIN_READ_GAP_MS);
    expect(readDelay(1000 + MIN_READ_GAP_MS * 2, 1000, 0.5)).toBe(READ_SPREAD_MS / 2);
  });

  it('keeps the owners users apart and clears them when a node has none left', () => {
    const nodes = [
      { id: 'a', clients: 1, other_clients: 5 },
      { id: 'b', clients: 2, other_clients: 6 },
      { id: 'c', clients: null, other_clients: 7 },
    ] as PublicNode[];
    const live: LiveTick = { at: at(0), total: 3, domains: {}, nodes: { a: 1, b: 2 }, other_nodes: { a: 40 } };
    const out = withLiveClients(nodes, [], live).nodes;
    expect(out.map((n) => n.other_clients)).toEqual([40, undefined, 7]);
    expect(out[0].clients).toBe(1);
  });
});
