<script lang="ts">
  // A small line of a number over time. A missing value (null) is a gap in the line: no data is
  // not zero.
  let { values }: { values: readonly (number | null)[] } = $props();

  const W = 100;
  const H = 24;

  const shape = $derived.by(() => {
    const known = values.filter((v): v is number => v !== null);
    if (known.length < 2) return null;
    const mx = Math.max(...known, 1);
    const mn = Math.min(...known);
    const step = W / Math.max(1, values.length - 1);
    const y = (v: number) => H - 2 - ((v - mn) / (mx - mn || 1)) * (H - 4);
    let line = '';
    let area = '';
    let run: [number, number][] = [];
    const flush = () => {
      if (run.length > 1) {
        const pts = run.map(([x, yy]) => `${x.toFixed(1)} ${yy.toFixed(1)}`);
        line += `M${pts.join('L')}`;
        area += `M${run[0][0].toFixed(1)} ${H}L${pts.join('L')}L${run[run.length - 1][0].toFixed(1)} ${H}Z`;
      }
      run = [];
    };
    values.forEach((v, i) => {
      if (v === null) flush();
      else run.push([i * step, y(v)]);
    });
    flush();
    return { line, area };
  });
</script>

{#if shape}
  <svg class="spark" viewBox="0 0 100 24" preserveAspectRatio="none" aria-hidden="true">
    <path class="a" d={shape.area} />
    <path class="l" d={shape.line} />
  </svg>
{/if}
