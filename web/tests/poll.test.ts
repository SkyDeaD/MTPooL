// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createPoller } from '../src/lib/poll';
import { fakeApi, FIXTURE_NOW, setHidden } from './helpers';

beforeEach(() => {
  vi.useFakeTimers({ now: FIXTURE_NOW });
  setHidden(false);
});
afterEach(() => {
  vi.useRealTimers();
});

describe('poller', () => {
  it('polling_uses_etag_and_30s_interval', async () => {
    const api = fakeApi();
    const onData = vi.fn();
    const poller = createPoller({ url: '/api/v1/nodes', fetchFn: api.fetchFn, onData, onError: vi.fn() });
    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(api.calls).toHaveLength(1);
    expect(api.calls[0].etag).toBeNull();
    expect(onData).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(29_999);
    expect(api.calls).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(api.calls).toHaveLength(2);
    // The second request names the tag of the first answer, and a 304 keeps the data as it was.
    expect(api.calls[1].etag).toMatch(/^"nodes-/);
    expect(onData).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(30_000);
    expect(api.calls).toHaveLength(3);
    poller.stop();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(api.calls).toHaveLength(3);
  });

  it('polling_pauses_when_hidden', async () => {
    const api = fakeApi();
    const poller = createPoller({ url: '/api/v1/nodes', fetchFn: api.fetchFn, onData: vi.fn(), onError: vi.fn() });
    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(api.calls).toHaveLength(1);

    setHidden(true);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(api.calls).toHaveLength(1);

    // Coming back refreshes at once instead of waiting for the next tick.
    setHidden(false);
    await vi.advanceTimersByTimeAsync(0);
    expect(api.calls).toHaveLength(2);
    poller.stop();
  });

  it('reports_a_failure_and_keeps_polling', async () => {
    const api = fakeApi();
    api.fail(true);
    const onError = vi.fn();
    const onData = vi.fn();
    const poller = createPoller({ url: '/api/v1/nodes', fetchFn: api.fetchFn, onData, onError });
    poller.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(onError).toHaveBeenCalledTimes(1);
    api.fail(false);
    await vi.advanceTimersByTimeAsync(30_000);
    expect(onData).toHaveBeenCalledTimes(1);
    poller.stop();
  });
});
