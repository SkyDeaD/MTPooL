// A node, as the registrar sees it: the calls the real agent makes, made by hand. The address a node
// has comes from `X-Forwarded-For` (the registrar trusts it from a loopback peer, as it does from
// its own reverse proxy), so the stand can give nodes documentation addresses (203.0.113.x).

export type FakesMode = 'globalping' | 'cheburcheck' | 'redflag' | 'cloudflare' | 'github';

export interface SimNode {
  name: string;
  id: string;
  token: string;
  ip: string;
  port: number;
}

export class Agent {
  private readonly base: string;
  private readonly fakesBase: string;
  private readonly enrollToken: string;

  constructor(base: string, fakesBase: string, enrollToken: string) {
    this.base = base;
    this.fakesBase = fakesBase;
    this.enrollToken = enrollToken;
  }

  private async call(path: string, bearer: string, ip: string, body: unknown): Promise<any> {
    const res = await fetch(`${this.base}/api/v1/agent${path}`, {
      method: 'POST',
      headers: { authorization: `Bearer ${bearer}`, 'content-type': 'application/json', 'x-forwarded-for': ip },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    if (!res.ok) throw new Error(`agent ${path}: ${res.status} ${text}`);
    return text ? JSON.parse(text) : null;
  }

  async enroll(name: string, ip: string, port: number): Promise<SimNode> {
    const got = await this.call('/enroll', this.enrollToken, ip, { name, port, engine: 'telemt' });
    return { name, id: got.node_id, token: got.node_token, ip, port };
  }

  heartbeat(node: SimNode): Promise<unknown> {
    return this.call('/heartbeat', node.token, node.ip, {});
  }

  /** Sets what a stand-in sensor answers. */
  async fakes(sensor: FakesMode, mode: string): Promise<void> {
    const res = await fetch(`${this.fakesBase}/__control`, {
      method: 'POST',
      body: JSON.stringify({ sensor, mode }),
    });
    if (!res.ok) throw new Error(`fakes ${sensor}=${mode}: ${res.status} ${await res.text()}`);
  }

  /** The agent's measurement: asks the stand-in Globalping for one, then reports its id. */
  async report(node: SimNode, opts: { clients?: number; other?: number; sni?: string } = {}): Promise<void> {
    const sni = opts.sni ?? 'tls.example.com';
    const made = await fetch(`${this.fakesBase}/globalping/v1/measurements`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        type: 'http',
        target: node.ip,
        locations: [{ country: 'RU', tags: ['eyeball-network'], limit: 20 }],
        measurementOptions: { port: node.port, protocol: 'HTTPS', request: { method: 'HEAD', host: sni, path: '/' } },
      }),
    });
    const id: string = (await made.json()).id;
    await this.call('/report', node.token, node.ip, {
      ip: node.ip,
      port: node.port,
      sni,
      gp: { kind: 'measured', id },
      metrics: {
        ok: true,
        from_client_total: 1_000_000 + (opts.clients ?? 0) * 1000,
        to_client_total: 5_000_000 + (opts.clients ?? 0) * 4000,
        clients: opts.clients ?? 0,
        ...(opts.other === undefined ? {} : { other_clients: opts.other, other_from_client_total: 0, other_to_client_total: 0 }),
      },
      dc2: { ok: true, reason: null },
      host: {
        load1: 0.4,
        mem_used_bytes: 400_000_000,
        mem_total_bytes: 2_000_000_000,
        disk_used_bytes: 5_000_000_000,
        disk_total_bytes: 40_000_000_000,
      },
      checked_at: new Date().toISOString().replace(/\.\d+Z$/, 'Z'),
      error: null,
    });
  }
}
