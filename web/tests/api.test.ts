import { describe, expect, it } from 'vitest';
import { ApiError, createClient } from '../src/lib/api';

type Call = { url: string; init: RequestInit };

function fake(responses: Array<Response | (() => Response)>) {
  const calls: Call[] = [];
  const impl = async (url: string | URL | Request, init: RequestInit = {}) => {
    calls.push({ url: String(url), init });
    const next = responses.shift();
    if (!next) throw new Error('no response left');
    return typeof next === 'function' ? next() : next;
  };
  return { calls, fetch: impl as typeof fetch };
}

const json = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json', ...headers },
  });

const problem = (status: number, code: string) =>
  new Response(JSON.stringify({ code, status, title: 'T', detail: 'D', type: 'about:blank' }), {
    status,
    headers: { 'content-type': 'application/problem+json' },
  });

describe('api client', () => {
  it('sends GET with the query and without a CSRF header', async () => {
    const f = fake([json([])]);
    const api = createClient(f.fetch);
    await api.get('/api/v1/stats/clients', { query: { range: '1h' } });
    expect(f.calls[0].url).toBe('/api/v1/stats/clients?range=1h');
    expect(f.calls[0].init.method).toBe('GET');
    const headers = new Headers(f.calls[0].init.headers);
    expect(headers.has('x-csrf-token')).toBe(false);
    expect(f.calls[0].init.credentials).toBe('same-origin');
  });

  it('logs in with the token and sends the CSRF token on changes only', async () => {
    const f = fake([json({ csrf: 'abc' }), json({}), json({})]);
    const api = createClient(f.fetch);
    await api.login('secret-token');
    expect(f.calls[0].url).toBe('/api/v1/admin/session');
    expect(f.calls[0].init.method).toBe('POST');
    expect(f.calls[0].init.body).toBe(JSON.stringify({ token: 'secret-token' }));

    await api.post('/api/v1/admin/nodes/{id}/recheck', { params: { id: 'n 1' } });
    expect(f.calls[1].url).toBe('/api/v1/admin/nodes/n%201/recheck');
    expect(new Headers(f.calls[1].init.headers).get('x-csrf-token')).toBe('abc');

    await api.get('/api/v1/admin/settings');
    expect(new Headers(f.calls[2].init.headers).has('x-csrf-token')).toBe(false);
  });

  it('puts a JSON body with its content type', async () => {
    const f = fake([json({ csrf: 'abc' }), json({})]);
    const api = createClient(f.fetch);
    await api.login('t');
    await api.patch('/api/v1/admin/settings', { body: { x: 1 } as never });
    const headers = new Headers(f.calls[1].init.headers);
    expect(headers.get('content-type')).toBe('application/json');
    expect(f.calls[1].init.body).toBe('{"x":1}');
  });

  it('restores the CSRF token of a reloaded panel', async () => {
    const f = fake([json({ csrf: 'again' }), json({})]);
    const api = createClient(f.fetch);
    expect(await api.restoreSession()).toBe(true);
    await api.delete('/api/v1/admin/tokens/{id}', { params: { id: '7' } });
    expect(new Headers(f.calls[1].init.headers).get('x-csrf-token')).toBe('again');
  });

  it('reports no session on 401 or 204', async () => {
    const api = createClient(fake([problem(401, 'unauthorized')]).fetch);
    expect(await api.restoreSession()).toBe(false);
    const bearer = createClient(fake([new Response(null, { status: 204 })]).fetch);
    expect(await bearer.restoreSession()).toBe(false);
  });

  it('turns a problem response into an ApiError with its code', async () => {
    const api = createClient(fake([problem(404, 'not_found')]).fetch);
    const error = await api.get('/api/v1/nodes/{id}', { params: { id: 'x' } }).catch((e) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect(error.status).toBe(404);
    expect(error.code).toBe('not_found');
  });

  it('calls the unauthorized hook on 401 and forgets the session', async () => {
    const f = fake([json({ csrf: 'abc' }), problem(401, 'unauthorized'), json({})]);
    let hooked = 0;
    const api = createClient(f.fetch, { onUnauthorized: () => hooked++ });
    await api.login('t');
    await api.get('/api/v1/admin/settings').catch(() => {});
    expect(hooked).toBe(1);
    await api.post('/api/v1/admin/nodes/{id}/recheck', { params: { id: '1' } });
    expect(new Headers(f.calls[2].init.headers).has('x-csrf-token')).toBe(false);
  });

  it('survives an error body that is not a problem', async () => {
    const api = createClient(fake([new Response('boom', { status: 502 })]).fetch);
    const error = await api.get('/api/v1/domains').catch((e) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect(error.status).toBe(502);
    expect(error.code).toBe('http_error');
  });

  it('returns undefined for 204', async () => {
    const f = fake([new Response(null, { status: 204 })]);
    const api = createClient(f.fetch);
    expect(await api.get('/api/v1/domains')).toBeUndefined();
  });
});
