<script lang="ts">
  import { t, tp } from '../lib/i18n';
  import { clock, ENGINE_NAMES, secondsUntil, shortDomain, type Scheme } from '../lib/model';

  // Domains on the left, their masters on the right. A folded domain hangs under the domain its
  // CNAME points to. A solid line is an A record, a dashed one a CNAME. `href` says where a node's
  // page is; the scheme knows nothing of routes.
  let { scheme, href, now }: { scheme: Scheme; href: (id: string) => string; now: number } = $props();

  const anyFold = $derived(scheme.domains.some((d) => d.folded.length > 0));
  // The dot animations are made for 1 to 8 masters of a domain.
  const kOf = (n: number) => Math.min(8, Math.max(1, n));
</script>

<section class="t">
  <div class="th">
    <h2>{t('scheme.title')}</h2>
    <span class="sub">{t('scheme.sub')}</span>
  </div>
  <div class="topo">
    <div class="hd"><span>{t('scheme.domains')}</span><span>{t('scheme.masters')}</span></div>
    {#each scheme.domains as d, di (d.domain)}
      {@const k = d.masters.length}
      <div class="dg">
        <div class="card dc">
          <div class="d">{d.domain}</div>
          {#if d.clients !== null}<div class="s" style="margin-top:4px">{t('scheme.clients', { n: d.clients })}</div>{/if}
          <div class="s">
            {#if k > 1}{t('scheme.recordsMany', { n: k, word: tp('unit.record', k) })}
            {:else if k === 1}{t('scheme.recordOne')}
            {:else}{t('scheme.noMaster')}{/if}
          </div>
        </div>
        {#each d.masters as m, j (m.id)}
          {@const left = secondsUntil(m.rotates_at, now)}
          <div class={['mi', j === 0 && 'first']} style="grid-row:{j + 1}">
            <i
              class="run md"
              style="animation:md{kOf(k)}_{Math.min(j, kOf(k) - 1)} {(kOf(k) * 3.2).toFixed(1)}s linear infinite;animation-delay:{(j * 3.2 + di * 0.7).toFixed(1)}s"
            ></i>
            <a class="card m" href={href(m.id)}>
              <div class="a">
                <span class="b">{m.id}</span>
                {#if typeof m.clients === 'number'}<span class="s nowrap">{t('scheme.clients', { n: m.clients })}</span>{/if}
              </div>
              <div class="s" style="margin-top:2px"><span class="mono">{m.ip}</span>, {ENGINE_NAMES[m.engine]}</div>
              {#if left !== null}<div class="s" style="margin-top:6px">{t('scheme.rotates', { time: clock(left) })}</div>{/if}
            </a>
          </div>
        {/each}
        {#if d.folded.length > 0}
          <div class="fds" style="grid-row:2/span {Math.max(1, k - 1)}">
            {#each d.folded as f, j (f.domain)}
              <div class="fd">
                <span class="ftk" style="--j:{j}"><i class="run fl" style="--dl:{(j * 0.9).toFixed(1)}s"></i></span>
                <div class="card fc">
                  <div class="a"><span class="d">{f.domain}</span><span class="tag">{t('scheme.folded')}</span></div>
                  <div class="s" style="margin-top:2px">{t('scheme.foldedText', { host: shortDomain(d.domain) })}</div>
                </div>
              </div>
            {/each}
          </div>
        {/if}
      </div>
    {/each}
    {#if anyFold}
      <div class="tlg faint xs">
        <span><i class="s1"></i>{t('scheme.legendA')}</span>
        <span><i class="s2"></i>{t('scheme.legendCname')}</span>
      </div>
    {/if}
    <div class="row faint sm" style="margin-top:8px">
      <span>{t('scheme.queue', { n: scheme.queue.length, word: tp('unit.node', scheme.queue.length) })}</span>
    </div>
    <div class="qlist">
      {#each scheme.queue as q, i (q.id)}
        <a class={['q', i === 0 && 'next']} href={href(q.id)}>
          <span class="p">{i + 1}</span><span class="x">{q.id}</span>
          {#if typeof q.clients === 'number'}<span class="y">{t('scheme.clients', { n: q.clients })}</span>{/if}
        </a>
      {/each}
    </div>
  </div>
</section>
