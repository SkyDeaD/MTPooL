// A thin typed client over the generated OpenAPI types (`api-types.ts`, made by
// `npm run gen:api`).
//
// The panel logs in with `POST /api/v1/admin/session`: the server sets an HttpOnly cookie and
// returns a CSRF token, which every change (anything but GET and HEAD) sends back in
// `X-CSRF-Token`. The cookie goes along by itself; the token is kept in memory and asked for
// again with `restoreSession()` after a reload. These session routes are not in the OpenAPI
// document, so they are typed here by hand.

import type { paths } from './api-types';

type Method = 'get' | 'post' | 'put' | 'patch' | 'delete';

type OperationOf<P extends keyof paths, M extends Method> = NonNullable<paths[P][M]>;

type JsonOf<R> = R extends { content: { 'application/json': infer B } } ? B : never;

type Ok<O> = O extends { responses: infer R }
  ? JsonOf<R[keyof R & (200 | 201)]> extends infer B
    ? [B] extends [never]
      ? void
      : B
    : never
  : void;

type QueryOf<O> = O extends { parameters: { query?: infer Q } } ? NonNullable<Q> : never;

type BodyOf<O> = O extends { requestBody?: infer B }
  ? NonNullable<B> extends { content: { 'application/json': infer J } }
    ? J
    : never
  : never;

export interface Init<O> {
  /** Values for the `{name}` parts of the path. */
  params?: Record<string, string | number>;
  query?: [QueryOf<O>] extends [never] ? never : QueryOf<O>;
  body?: [BodyOf<O>] extends [never] ? never : BodyOf<O>;
  signal?: AbortSignal;
}

/** An answer of the server that is not a success. `code` is the stable part to branch on. */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly detail: string;

  constructor(status: number, code: string, title: string, detail = '') {
    super(title);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.detail = detail;
  }
}

export interface ClientOptions {
  /** Called when the server says the session is gone (401). */
  onUnauthorized?: () => void;
}

const SESSION_PATH = '/api/v1/admin/session';
const CSRF_HEADER = 'x-csrf-token';

export function createClient(fetchImpl: typeof fetch = (...a) => fetch(...a), options: ClientOptions = {}) {
  let csrf: string | null = null;

  async function send(method: string, url: string, body?: unknown, signal?: AbortSignal) {
    const headers = new Headers({ accept: 'application/json' });
    if (body !== undefined) headers.set('content-type', 'application/json');
    if (csrf && method !== 'GET' && method !== 'HEAD') headers.set(CSRF_HEADER, csrf);
    const response = await fetchImpl(url, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      credentials: 'same-origin',
      signal,
    });
    if (response.ok) return response;
    if (response.status === 401) {
      csrf = null;
      options.onUnauthorized?.();
    }
    throw await toError(response);
  }

  async function parse<T>(response: Response): Promise<T> {
    if (response.status === 204) return undefined as T;
    const text = await response.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }

  async function request<P extends keyof paths, M extends Method>(
    method: M,
    path: P,
    init: Init<OperationOf<P, M>> = {},
  ): Promise<Ok<OperationOf<P, M>>> {
    const response = await send(method.toUpperCase(), buildUrl(path, init), init.body, init.signal);
    return parse(response);
  }

  return {
    get: <P extends keyof paths>(path: P, init?: Init<OperationOf<P, 'get'>>) =>
      request('get', path, init),
    post: <P extends keyof paths>(path: P, init?: Init<OperationOf<P, 'post'>>) =>
      request('post', path, init),
    put: <P extends keyof paths>(path: P, init?: Init<OperationOf<P, 'put'>>) =>
      request('put', path, init),
    patch: <P extends keyof paths>(path: P, init?: Init<OperationOf<P, 'patch'>>) =>
      request('patch', path, init),
    delete: <P extends keyof paths>(path: P, init?: Init<OperationOf<P, 'delete'>>) =>
      request('delete', path, init),

    /** Exchanges the admin token for a session. */
    async login(token: string): Promise<void> {
      const response = await send('POST', SESSION_PATH, { token });
      csrf = (await parse<{ csrf: string }>(response)).csrf;
    },

    /**
     * After a reload: is there a live session, and if so keep its CSRF token. A 401 or a 204
     * (a bearer token has no session) means there is none.
     */
    async restoreSession(): Promise<boolean> {
      try {
        const response = await send('GET', SESSION_PATH);
        if (response.status === 204) return false;
        csrf = (await parse<{ csrf: string }>(response)).csrf;
        return true;
      } catch (error) {
        if (error instanceof ApiError && error.status === 401) return false;
        throw error;
      }
    },

    async logout(): Promise<void> {
      try {
        await send('DELETE', SESSION_PATH);
      } finally {
        csrf = null;
      }
    },
  };
}

function buildUrl(path: string, init: { params?: Record<string, string | number>; query?: unknown }) {
  const filled = path.replace(/\{(\w+)\}/g, (_, name: string) => {
    const value = init.params?.[name];
    if (value === undefined) throw new Error(`missing path parameter ${name} for ${path}`);
    return encodeURIComponent(String(value));
  });
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries((init.query ?? {}) as Record<string, unknown>)) {
    if (value !== undefined && value !== null) query.set(key, String(value));
  }
  const text = query.toString();
  return text ? `${filled}?${text}` : filled;
}

async function toError(response: Response): Promise<ApiError> {
  try {
    const body = await response.json();
    if (body && typeof body.code === 'string') {
      return new ApiError(response.status, body.code, body.title ?? body.code, body.detail ?? '');
    }
  } catch {
    // Not JSON: fall through to the generic error.
  }
  return new ApiError(response.status, 'http_error', `HTTP ${response.status}`);
}

/** The client the pages use. */
export const api = createClient();
