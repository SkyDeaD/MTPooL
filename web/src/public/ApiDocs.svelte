<script lang="ts">
  import { t } from '../lib/i18n';
  import { loadRapidoc } from '../lib/rapidoc';
  import { toast } from '../lib/toast.svelte';
  import { messages, type MessageKey } from '../strings.ru';
  import Icon from '../shared/Icon.svelte';
  import { botRoutes, publicRoutes, type ApiRoute } from './api-routes.generated';
  import PublicShell from './PublicShell.svelte';

  // The API page: what anyone can ask without a token, what a bot can do with one, webhooks and
  // errors. The routes come from docs/openapi.json (scripts/gen-api-docs.ts); the words are ours.
  // Trying a request needs RapiDoc, a 640 KB script that is loaded only when asked for.
  const sections = ['quick', 'pub', 'bot', 'hooks', 'errs'] as const;
  const events = ['node.registered', 'node.quarantined', 'node.recovered', 'node.banned', 'node.dead', 'node.pruned', 'ban.lifted', 'master.changed'];

  const quick = `curl -s "$MTPOOL_URL/api/v1/nodes" | jq '.[] | {id, state}'`;
  const botSample = `curl -s -X POST "$MTPOOL_URL/api/v1/me/nodes/ID/recheck" \\\n  -H "Authorization: Bearer $MTPOOL_BOT_TOKEN"`;
  const verify = `import hmac, hashlib

def verify(secret: bytes, body: bytes, header: str) -> bool:
    parts = dict(p.split("=", 1) for p in header.split(","))
    signed = parts["t"].encode() + b"." + body
    expected = hmac.new(secret, signed, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, parts["v1"])`;
  const problem = `{
  "type": "about:blank",
  "title": "Not found",
  "status": 404,
  "code": "not_found",
  "detail": "No node with this id."
}`;

  const describe = (r: ApiRoute) => {
    const key = `api.route.${r.method} ${r.path}`;
    return key in messages ? t(key as MessageKey) : '';
  };
  const methodClass = (m: string) => (m === 'POST' ? 'post' : m === 'DELETE' ? 'del' : '');

  let tried = $state<string | null>(null);
  let failed = $state(false);
  async function tryIt(route: ApiRoute): Promise<void> {
    failed = false;
    try {
      await loadRapidoc();
      tried = `${route.method} ${route.path}`;
    } catch {
      failed = true;
    }
  }

  // RapiDoc shows one operation of our own OpenAPI document, nothing is fetched from elsewhere.
  function rapi(node: HTMLElement, route: ApiRoute) {
    const el = document.createElement('rapi-doc');
    el.setAttribute('spec-url', '/api/v1/openapi.json');
    el.setAttribute('render-style', 'focused');
    el.setAttribute('goto-path', `${route.method.toLowerCase()}-${route.path}`);
    el.setAttribute('show-header', 'false');
    el.setAttribute('allow-authentication', 'false');
    el.setAttribute('allow-server-selection', 'false');
    el.setAttribute('theme', document.documentElement.dataset.theme === 'light' ? 'light' : 'dark');
    node.append(el);
    return { destroy: () => el.remove() };
  }

  async function copy(text: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      toast(t('api.copied'));
    } catch {
      toast(t('links.copyFailed'));
    }
  }
</script>

{#snippet code(text: string)}
  <div class="codew">
    <pre class="code" tabindex="0">{text}</pre>
    <button class="btn s ic" type="button" aria-label={t('api.copy')} onclick={() => copy(text)}><Icon name="copy" /></button>
  </div>
{/snippet}

{#snippet route(r: ApiRoute, canTry: boolean)}
  <div class="rt">
    <div class="p">
      <span class="mth {methodClass(r.method)}">{r.method}</span>
      <code>{r.path}</code>
      {#if r.query.length > 0}<span class="faint sm">?{r.query.join('&')}</span>{/if}
      {#if canTry}
        <span class="sp"></span>
        <button class="btn s" type="button" data-tryit onclick={() => tryIt(r)}>{t('api.try')}</button>
      {/if}
    </div>
    <span class="muted sm">{describe(r)}</span>
    {#if tried === `${r.method} ${r.path}`}<div use:rapi={r}></div>{/if}
  </div>
{/snippet}

<PublicShell active="api">
  <div class="ph">
    <h1>{t('api.title')}</h1>
    <span class="sub">{t('api.sub')}</span>
  </div>
  <div class="api">
    <nav class="toc" aria-label={t('api.toc')}>
      <span class="lb">{t('api.onPage')}</span>
      {#each sections as s (s)}<a href="#sec-{s}">{t(`api.sec.${s}`)}</a>{/each}
    </nav>
    <div class="stack">
      <section class="t" id="sec-quick" style="--i:0">
        <div class="th"><h2>{t('api.sec.quick')}</h2></div>
        <div class="tb-b stack">
          <p class="muted" style="max-width:70ch">{t('api.quick.text')}</p>
          {@render code(quick)}
        </div>
      </section>

      <section class="t" id="sec-pub" style="--i:1">
        <div class="th"><h2>{t('api.sec.pub')}</h2><span class="sub">{t('api.pub.sub')}</span></div>
        {#each publicRoutes as r (r.method + r.path)}{@render route(r, true)}{/each}
        {#if failed}<p class="stale">{t('api.tryFailed')}</p>{/if}
      </section>

      <section class="t" id="sec-bot" style="--i:2">
        <div class="th"><h2>{t('api.sec.bot')}</h2><span class="sub">{t('api.bot.sub')}</span></div>
        <div class="tb-b stack">
          <div class="prose"><p>{t('api.bot.text')}</p></div>
          {@render code(botSample)}
        </div>
        {#each botRoutes as r (r.method + r.path)}{@render route(r, false)}{/each}
      </section>

      <section class="t" id="sec-hooks" style="--i:3">
        <div class="th"><h2>{t('api.sec.hooks')}</h2><span class="sub">{t('api.hooks.sub')}</span></div>
        <div class="tb-b stack">
          <div class="prose">
            <p><b>{t('api.hooks.events')}</b> {#each events as e, i (e)}<code>{e}</code>{i < events.length - 1 ? ', ' : '.'}{/each}</p>
            <p>{t('api.hooks.sign')} <code>X-MTPooL-Signature</code>: <code>t=&lt;ts&gt;,v1=&lt;hex&gt;</code>. {t('api.hooks.signText')}</p>
          </div>
          {@render code(verify)}
        </div>
      </section>

      <section class="t" id="sec-errs" style="--i:4">
        <div class="th"><h2>{t('api.sec.errs')}</h2></div>
        <div class="tb-b stack">
          <p class="muted" style="max-width:70ch">{t('api.errs.text')}</p>
          {@render code(problem)}
        </div>
      </section>
    </div>
  </div>
</PublicShell>
