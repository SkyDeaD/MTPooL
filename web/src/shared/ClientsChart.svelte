<script lang="ts">
  import { formatDay, formatHour, formatNumber, formatSecond, formatWhen } from '../lib/dates';
  import { t } from '../lib/i18n';
  import { countTicks, monotonePath, niceStep, round1 } from '../lib/chart';
  import type { ClientsRange, ClientsSeries } from '../lib/types';
  import Icon from './Icon.svelte';
  import Seg from './Seg.svelte';

  // The clients of the pool over time. The line is drawn from the points it was given and, on the
  // 10 minute range, its head moves on between two of them: the picture runs `delayMs` behind real
  // time (one step of the series when no stream feeds it), so the head always sits between known
  // values and never guesses ahead.
  let {
    series,
    range,
    onrange,
    delayMs,
  }: {
    series: ClientsSeries | null;
    range: ClientsRange;
    onrange: (range: ClientsRange) => void;
    delayMs?: number;
  } = $props();

  // The line is drawn anew only in answer to the reader, never because a page opened.
  let asked = $state(false);
  function pick(next: ClientsRange): void {
    asked = true;
    onrange(next);
  }

  type Mode = 'total' | 'stack';
  let mode = $state<Mode>('total');

  const REST = '_rest';
  const SPAN_MS: Record<ClientsRange, number> = { '10m': 600e3, '1h': 3600e3, '24h': 864e5, '7d': 6048e5 };
  const TICK_MS: Record<ClientsRange, number> = { '10m': 120e3, '1h': 600e3, '24h': 144e5, '7d': 864e5 };
  const MT = 14;
  const MB = 22;

  let width = $state(0);
  let height = $state(150);
  let hoverX = $state<number | null>(null);
  let hoverY = $state(0);
  let nowMs = $state(Date.now());
  let eased = $state.raw<{ lo: number; hi: number } | null>(null);
  let reduced = $state(false);

  interface Pt {
    at: number;
    total: number;
    parts: Record<string, number>;
  }

  const pts = $derived.by<Pt[]>(() => {
    if (!series) return [];
    return series.points.map((p) => {
      const parts: Record<string, number> = { ...p.domains };
      const known = Object.values(p.domains).reduce((a, b) => a + b, 0);
      parts[REST] = Math.max(0, p.total - known);
      return { at: Date.parse(p.at), total: p.total, parts };
    });
  });

  const stepMs = $derived((series?.step_sec ?? 20) * 1000);
  const live = $derived(range === '10m' && !reduced && pts.length > 1);
  const lastAt = $derived(pts.length > 0 ? pts[pts.length - 1].at : 0);
  // The right edge of the picture. Live, it follows the clock one step behind the newest point.
  const tEnd = $derived(live ? Math.min(nowMs - (delayMs ?? stepMs), lastAt) : lastAt);
  const t0 = $derived(tEnd - SPAN_MS[range]);

  const phone = $derived(width < 520);
  const ml = $derived(phone ? 40 : 50);
  const mr = $derived(phone ? 52 : 62);
  const plotW = $derived(Math.max(1, width - ml - mr));
  const plotH = $derived(Math.max(1, height - MT - MB));
  const X = (at: number) => ml + ((at - t0) / SPAN_MS[range]) * plotW;

  // Points inside the window, plus the one just before it, so the line enters from the left edge.
  const visible = $derived.by(() => {
    const out: Pt[] = [];
    for (let i = 0; i < pts.length; i++) {
      const p = pts[i];
      const next = pts[i + 1];
      if (p.at < t0 && next && next.at < t0) continue;
      if (p.at > tEnd) break;
      out.push(p);
    }
    return out;
  });

  const zero = $derived(mode === 'stack' || range === '24h' || range === '7d');

  const target = $derived.by(() => {
    const vals = visible.map((p) => p.total);
    if (vals.length === 0) return { lo: 0, hi: 1 };
    // The next point is about to be drawn; the scale has room for it before the line gets there.
    const ahead = pts.find((p) => p.at > tEnd);
    if (ahead) vals.push(ahead.total);
    let mn = Math.min(...vals);
    const mx = Math.max(...vals);
    if (zero) mn = 0;
    const pad = zero ? mx * 0.04 : Math.max((mx - mn) * 0.18, Math.max(mx, 1) * 0.004);
    const step = niceStep((mx - mn + 2 * pad) / 2);
    return {
      lo: zero ? 0 : Math.max(0, mn - pad),
      hi: zero ? Math.ceil((mx + pad) / step) * step : mx + pad,
    };
  });

  const yr = $derived(live && eased ? eased : target);
  const yLo = $derived(yr.lo);
  const yHi = $derived(Math.max(yr.hi, yr.lo + 1));
  const Y = (v: number) => MT + (1 - (v - yLo) / (yHi - yLo)) * plotH;

  const yTicks = $derived(countTicks(yLo, yHi));

  const xTicks = $derived.by(() => {
    const iv = TICK_MS[range] * (phone ? 2 : 1);
    const out: { x: number; label: string }[] = [];
    // Ticks fall on round hours of the reader's clock.
    const zone = -new Date(t0).getTimezoneOffset() * 60_000;
    for (let at = Math.floor((t0 + zone) / iv) * iv - zone; at <= tEnd && out.length < 10; at += iv) {
      const x = X(at);
      if (x < ml + 16 || x > width - mr - 10) continue;
      out.push({ x, label: range === '7d' ? formatDay(at) : formatHour(at) });
    }
    return out;
  });

  // The value at a moment, between the two points around it.
  function between(at: number, pick: (p: Pt) => number): number {
    if (visible.length === 0) return 0;
    for (let i = 0; i < visible.length - 1; i++) {
      const a = visible[i];
      const b = visible[i + 1];
      if (a.at <= at && b.at >= at) {
        const span = b.at - a.at;
        return span <= 0 ? pick(b) : pick(a) + ((pick(b) - pick(a)) * (at - a.at)) / span;
      }
    }
    return pick(visible[visible.length - 1]);
  }

  // A step without samples has no point, so a long hole breaks the line.
  const runs = $derived.by(() => {
    const cut = visible.filter((p) => p.at <= tEnd);
    const out: Pt[][] = [];
    let run: Pt[] = [];
    for (const p of cut) {
      const prev = run[run.length - 1];
      if (prev && p.at - prev.at > stepMs * 2.5) {
        out.push(run);
        run = [];
      }
      run.push(p);
    }
    if (run.length > 0) out.push(run);
    return out;
  });

  const headX = $derived(X(tEnd));
  const headTotal = $derived(between(tEnd, (p) => p.total));
  const headY = $derived(Y(headTotal));

  const lines = $derived.by(() => {
    if (mode !== 'total') return [];
    return runs.map((run, i) => {
      const xy: [number, number][] = run.map((p) => [X(p.at), Y(p.total)]);
      if (i === runs.length - 1) xy.push([headX, headY]);
      const d = monotonePath(xy);
      const area = zero ? `${d}L${round1(xy[xy.length - 1][0])} ${round1(MT + plotH)}L${round1(xy[0][0])} ${round1(MT + plotH)}Z` : '';
      return { d, area };
    });
  });

  const keys = $derived.by(() => {
    const names = new Set<string>();
    for (const p of visible) for (const [k, v] of Object.entries(p.parts)) if (k !== REST && v > 0) names.add(k);
    const list = [...names].sort().slice(0, 4);
    return [...list, REST];
  });

  const bands = $derived.by(() => {
    if (mode !== 'stack') return [];
    const out: { key: string; d: string }[] = [];
    for (const [ri, run] of runs.entries()) {
      const last = ri === runs.length - 1;
      const cum = run.map(() => 0);
      let headCum = 0;
      for (const key of keys) {
        const bottom = cum.slice();
        const headBottom = headCum;
        run.forEach((p, j) => {
          cum[j] += p.parts[key] ?? 0;
        });
        headCum += between(tEnd, (p) => p.parts[key] ?? 0);
        const top = run.map((p, j) => `${round1(X(p.at))} ${round1(Y(cum[j]))}`);
        let d = `M${top.join('L')}`;
        if (last) d += `L${round1(headX)} ${round1(Y(headCum))}L${round1(headX)} ${round1(Y(headBottom))}`;
        for (let j = run.length - 1; j >= 0; j--) d += `L${round1(X(run[j].at))} ${round1(Y(bottom[j]))}`;
        out.push({ key, d: `${d}Z` });
      }
    }
    return out;
  });

  const seriesClass = (key: string) => {
    if (key === REST) return 'sr0';
    return `sr${keys.indexOf(key) + 1}`;
  };
  const seriesName = (key: string) => (key === REST ? t('clients.rest') : key);

  const marks = $derived.by(() => {
    if (!series) return [];
    return series.marks
      .map((m) => ({ at: Date.parse(m.at), kind: m.kind }))
      .filter((m) => m.at >= t0 && m.at <= tEnd && (m.kind === 'ban' || range === '10m' || range === '1h'))
      .slice(-14);
  });
  const markClass = { master_change: '', fold: 'warn', ban: 'bad' } as const;

  const nowTotal = $derived(pts.length > 0 ? pts[pts.length - 1].total : null);
  const delta = $derived.by(() => {
    if (nowTotal === null) return '';
    const from = pts.find((p) => p.at >= lastAt - SPAN_MS[range]) ?? pts[0];
    const d = nowTotal - from.total;
    if (d === 0) return `${t('clients.same')} ${t(`clients.of.${range}`)}`;
    return `${d > 0 ? '+' : '−'}${formatNumber(Math.abs(d))} ${t(`clients.of.${range}`)}`;
  });

  // Where the pointer is, the nearest point.
  const hover = $derived.by(() => {
    if (hoverX === null || hoverX < ml || hoverX > width - mr) return null;
    const cut = visible.filter((p) => p.at <= tEnd);
    if (cut.length === 0) return null;
    const at = t0 + ((hoverX - ml) / plotW) * SPAN_MS[range];
    const best = cut.reduce((a, b) => (Math.abs(b.at - at) < Math.abs(a.at - at) ? b : a));
    const near = marks.filter((m) => Math.abs(m.at - best.at) < SPAN_MS[range] / 50);
    return { point: best, x: X(best.at), near };
  });

  const tableRows = $derived.by(() => {
    const inside = pts.filter((p) => p.at >= lastAt - SPAN_MS[range]);
    const step = Math.max(1, Math.floor(inside.length / 12));
    return inside.filter((_, i) => (inside.length - 1 - i) % step === 0).slice(-12).reverse();
  });
  const when = (at: number) => (range === '10m' ? formatSecond(at) : formatWhen(at));

  const rangeItems = (['10m', '1h', '24h', '7d'] as const).map((value) => ({ value, label: t(`clients.range.${value}`) }));
  const modeItems = [
    { value: 'total' as Mode, label: t('clients.mode.total') },
    { value: 'stack' as Mode, label: t('clients.mode.stack') },
  ];

  const tagY = $derived(Math.max(0, Math.min(height - 24, headY - 11)));

  $effect(() => {
    reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  });

  // The picture is redrawn from the clock while it is live and the page is in view.
  $effect(() => {
    if (!live) {
      eased = null;
      return;
    }
    let frame = 0;
    let lastPaint = 0;
    const tick = (ts: number) => {
      frame = requestAnimationFrame(tick);
      if (document.hidden || ts - lastPaint < 33) return;
      lastPaint = ts;
      nowMs = Date.now();
      const goal = target;
      eased = eased ? { lo: eased.lo + (goal.lo - eased.lo) * 0.07, hi: eased.hi + (goal.hi - eased.hi) * 0.07 } : goal;
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  });

  // A new range or mode starts from its own scale.
  $effect(() => {
    void range;
    void mode;
    eased = null;
  });

  function point(event: PointerEvent): void {
    const box = (event.currentTarget as SVGElement).getBoundingClientRect();
    hoverX = event.clientX - box.left;
    hoverY = event.clientY;
  }
</script>

<section class="t">
  <div class="th cth">
    <div class="chv">
      <span class="l">{t('clients.title')}</span>
      <span class="v">{nowTotal === null ? '—' : formatNumber(nowTotal)}</span>
      <span class="s">{delta}</span>
    </div>
    <span class="sp"></span>
    <div class="row w">
      <Seg items={modeItems} value={mode} onchange={(v) => (mode = v)} label={t('clients.mode.label')} />
      <Seg items={rangeItems} value={range} onchange={pick} label={t('clients.range.label')} />
    </div>
  </div>
  <div class="cbody">
    {#if mode === 'stack' && nowTotal !== null}
      <div class="clg">
        {#each keys as key (key)}
          <span class="lg"><i class={seriesClass(key)}></i>{seriesName(key)} <b>{formatNumber(pts[pts.length - 1].parts[key] ?? 0)}</b></span>
        {/each}
      </div>
    {/if}
    <div
      class="cc"
      bind:clientWidth={width}
      bind:clientHeight={height}
      role="img"
      aria-label={nowTotal === null ? t('clients.empty') : t('clients.aria', { n: formatNumber(nowTotal), delta })}
    >
      {#if series === null}
        <div class="sk" style="height:100%"></div>
      {:else if pts.length === 0}
        <div class="ch-empty"><div><span>{t('clients.empty')}</span></div></div>
      {:else if width > 0}
        <svg
          class="ccs"
          viewBox="0 0 {width} {height}"
          aria-hidden="true"
          onpointermove={point}
          onpointerdown={point}
          onpointerleave={() => (hoverX = null)}
        >
          {#each yTicks as v (v)}
            <line class="gl" x1={ml} x2={width - mr} y1={Y(v)} y2={Y(v)} />
            <text class="ty" x={ml - 8} y={Y(v) + 4}>{formatNumber(v)}</text>
          {/each}
          {#each xTicks as tick (tick.x)}
            <text class="tx" x={tick.x} y={height - 7}>{tick.label}</text>
          {/each}
          <line class="bl" x1={ml} x2={width - mr} y1={height - MB} y2={height - MB} />
          {#key range}
          <g class={['plot', asked && 'wipe']}>
            {#each bands as band, i (i)}
              <path class="bd {seriesClass(band.key)}" d={band.d} />
            {/each}
            {#each lines as line, i (i)}
              {#if line.area}<path class="fa" d={line.area} />{/if}
              <path class="ln" d={line.d} />
            {/each}
            <!-- Two masters often change at the same instant: the time is not a key. -->
            {#each marks as m, i (i)}
              <line class="mk {markClass[m.kind]}" x1={X(m.at)} x2={X(m.at)} y1={MT} y2={height - MB} />
            {/each}
          </g>
          {/key}
          {#if hover}
            <line class="cross" x1={hover.x} x2={hover.x} y1={MT} y2={height - MB} />
            <circle class="cdot" r="4" cx={hover.x} cy={Y(hover.point.total)} />
          {/if}
          <circle class="pulse" r="4" cx={headX} cy={headY} />
          <circle class="head" r="4" cx={headX} cy={headY} />
        </svg>
        <div class="ctag" style="transform:translateY({tagY.toFixed(1)}px)">{formatNumber(nowTotal ?? 0)}</div>
      {/if}
    </div>
    {#if hover}
      <div class="tip ctip" style="left:{Math.max(8, hover.x + 14)}px;top:{Math.max(8, hoverY - 60)}px">
        <div class="tw2">{when(hover.point.at)}</div>
        <div class="ti tot"><span>{t('clients.tip.total')}</span><b>{formatNumber(hover.point.total)}</b></div>
        {#if mode === 'stack'}
          {#each keys as key (key)}
            <div class="ti"><i class={seriesClass(key)}></i><span>{seriesName(key)}</span><b>{formatNumber(hover.point.parts[key] ?? 0)}</b></div>
          {/each}
        {/if}
        {#each hover.near as m, i (i)}<div class="ti m">{t(`clients.mark.${m.kind}`)}</div>{/each}
      </div>
    {/if}
    <div class="cft">
      <div class="mlg xs">
        <span><i></i>{t('clients.mark.master_change')}</span>
        <span><i class="warn"></i>{t('clients.mark.fold')}</span>
        <span><i class="bad"></i>{t('clients.mark.ban')}</span>
      </div>
      <details class="cct">
        <summary class="xs"><Icon name="chev" cls="chev" />{t('clients.table')}</summary>
        {#if tableRows.length > 0}
          <div class="tw">
            <table class="tb">
              <thead><tr><th>{t('clients.col.when')}</th><th class="n">{t('clients.col.clients')}</th></tr></thead>
              <tbody>
                {#each tableRows as p (p.at)}
                  <tr><td>{when(p.at)}</td><td class="n">{formatNumber(p.total)}</td></tr>
                {/each}
              </tbody>
            </table>
          </div>
        {/if}
      </details>
    </div>
  </div>
</section>
