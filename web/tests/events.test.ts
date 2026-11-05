import { describe, expect, it } from 'vitest';
import { describeEvent } from '../src/panel/events';
import { STREAM_KINDS } from '../src/panel/stream';
import type { EventEntry } from '../src/panel/types';

const event = (kind: string, detail: unknown, node: string | null = 'n1', domain: string | null = null): EventEntry =>
  ({ id: 1, at: '2026-11-05T10:00:00Z', kind, node, domain, detail }) as unknown as EventEntry;

describe('the journal says what happened and why', () => {
  it('names the sensors behind a ban', () => {
    const d = describeEvent(
      event('node_banned', {
        votes: [
          { sensor: 'globalping', vote: 'blocked' },
          { sensor: 'redflag', vote: 'blocked' },
          { sensor: 'cheburcheck', vote: 'clear' },
        ],
      }),
    );
    expect(d.title).toBe('n1 забанена');
    expect(d.detail).toContain('Globalping, RedFlag');
    expect(d.detail).not.toContain('cheburcheck');
    expect(d.detail).toContain('Запись и токен удалены');
  });

  it('puts the words of the conditions on a quarantine', () => {
    const d = describeEvent(event('node_quarantined', { reason: ['report_stale', 'tcp'] }));
    expect(d.title).toBe('n1 в карантине');
    expect(d.detail).toBe('Нет свежего замера Globalping, порт не отвечает снаружи');
  });

  it('tells a silent node, a retired one, a refused address and a replacement apart', () => {
    expect(describeEvent(event('node_dead', { reason: 'unreachable' })).detail).toContain('Агент');
    expect(describeEvent(event('node_dead', { retired: true })).title).toBe('n1 ушла из пула');
    expect(describeEvent(event('node_dead', { reason: 'address_refused' })).detail).toContain('заблокирован');
    const silent = describeEvent(event('node_dead', { replaced_by: 'n2', reason: 'silent' }));
    expect(silent.title).toBe('n1 заменена нодой n2');
    expect(silent.detail).toContain('молчала');
    expect(describeEvent(event('node_dead', { replaced_by: 'n2' })).detail).toContain('тот же адрес');
  });

  it('shows both sides of a change of master', () => {
    const both = describeEvent(event('master_changed', { added: 'n2', removed: 'n1' }, null, 'a.example'));
    expect(both.title).toBe('Новый мастер у a.example');
    expect(both.detail).toBe('n2 вместо n1');
    expect(describeEvent(event('master_changed', { added: 'n2', removed: null }, null, 'a.example')).detail).toBe('n2');
    const gone = describeEvent(event('master_changed', { added: null, removed: 'n1' }, null, 'a.example'));
    expect(gone.title).toBe('Мастер снят у a.example');
    expect(gone.detail).toBe('n1');
  });

  it('shows the address of a registration and of a move', () => {
    const fresh = describeEvent(event('node_registered', { reverify: false, replaced: null, ip: '192.0.2.1' }));
    expect(fresh.title).toBe('n1 зарегистрирована');
    expect(fresh.detail).toContain('192.0.2.1');
    const moved = describeEvent(event('node_ip_changed', { from: '192.0.2.1', to: '192.0.2.2' }));
    expect(moved.title).toBe('n1 сменила адрес');
    expect(moved.detail).toBe('С 192.0.2.1 на 192.0.2.2');
    expect(moved.category).toBe('nodes');
    expect(STREAM_KINDS).toContain('node_ip_changed');
    expect(describeEvent(event('node_registered', { reverify: true, replaced: null, ip: '192.0.2.1' })).detail).toContain('проверка');
    expect(describeEvent(event('node_registered', { reverify: false, replaced: 'n0', ip: '192.0.2.1' })).detail).toContain('n0');
  });

  it('says who lifted a ban', () => {
    expect(describeEvent(event('ban_lifted', { admin: true })).detail).toContain('Администратор');
    expect(describeEvent(event('ban_lifted', {})).detail).toContain('проверк');
  });

  it('gives the numbers of DNS, reverify and clock events', () => {
    expect(describeEvent(event('dns_applied', { records: 2, addresses: ['192.0.2.1', '192.0.2.2'] }, null, 'a.example')).detail).toBe(
      'Адреса: 192.0.2.1, 192.0.2.2',
    );
    expect(describeEvent(event('dns_applied', { records: 1, target: 'b.example' }, null, 'a.example')).detail).toBe('CNAME на b.example');
    const failed = describeEvent(event('dns_failed', { error: 'boom', retry_in_sec: 60 }, null, 'a.example'));
    expect(failed.title).toBe('Ошибка DNS у a.example');
    expect(failed.detail).toContain('boom');
    expect(failed.detail).toContain('Повтор через');
    expect(describeEvent(event('node_reverify_failed', { retry_after_sec: 900 })).detail).toContain('Следующая попытка через');
    expect(describeEvent(event('clock_skew', { skew_sec: 120 })).detail).toContain('120');
    expect(describeEvent(event('store_error', { error: 'disk' }, null)).detail).toBe('disk');
  });

  it('counts a round by how the sensors voted', () => {
    const d = describeEvent(
      event('sensor_round', {
        verdict: 'undecided',
        votes: [
          { sensor: 'globalping', vote: 'blocked' },
          { sensor: 'redflag', vote: 'clear' },
          { sensor: 'cheburcheck', vote: { abstain: 'timeout' } },
        ],
      }),
    );
    expect(d.detail).toContain('Нет решения');
    expect(d.detail).toContain('Globalping');
    expect(d.detail).toContain('воздержались');
    expect(d.haystack).toContain('n1');
  });

  it('explains the actions of the reserve domains', () => {
    expect(describeEvent(event('srmd_action', { action: 'fold', target: 'b.example' }, null, 'a.example')).detail).toContain('b.example');
    expect(describeEvent(event('srmd_action', { action: 'take', unfolded: false }, null, 'a.example')).title).not.toBe('Событие');
  });

  it('does not break on a kind it does not know', () => {
    expect(describeEvent(event('from_the_future', { x: 1 })).title).toBe('Событие');
  });
});
