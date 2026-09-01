import * as React from "react";
import { cn } from "@/lib/cn";
import { num } from "@/lib/format";

/**
 * The claim on this page is that a filter is an intersection and a count is a
 * popcount. Rather than assert it, this runs it: three real 1,048,576-bit sets
 * are ANDed word by word and the survivors counted, in the reader's own
 * browser, on every toggle. The timing is measured, not written.
 */

const WORDS = 32_768;              // 1,048,576 bits
const BITS = WORDS * 32;
const COLS = 64, ROWS = 32;        // the grid renders the first 2,048 records

const PREDICATES = [
  { label: 'country = "GB"', seed: 11, density: 0.34 },
  { label: 'device  = "mobile"', seed: 29, density: 0.41 },
  { label: "converted = true", seed: 97, density: 0.72 },
];

function mulberry32(a: number) {
  return () => {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function makeSet(seed: number, density: number) {
  const r = mulberry32(seed);
  const w = new Uint32Array(WORDS);
  for (let i = 0; i < WORDS; i++) {
    let v = 0;
    for (let b = 0; b < 32; b++) if (r() < density) v |= 1 << b;
    w[i] = v >>> 0;
  }
  return w;
}

/** Hamming weight of a 32-bit word — the whole of what a count costs. */
function popcount(x: number) {
  x = x - ((x >>> 1) & 0x55555555);
  x = (x & 0x33333333) + ((x >>> 2) & 0x33333333);
  x = (x + (x >>> 4)) & 0x0f0f0f0f;
  return Math.imul(x, 0x01010101) >>> 24;
}

export function BitDemo() {
  const [on, setOn] = React.useState([true, true, false]);
  const [stats, setStats] = React.useState({ count: 0, us: 0, words: 0, runs: 0 });
  const canvas = React.useRef<HTMLCanvasElement>(null);
  const holder = React.useRef<HTMLDivElement>(null);
  const sets = React.useRef<Uint32Array[]>();
  const out = React.useRef(new Uint32Array(WORDS));

  if (!sets.current) sets.current = PREDICATES.map((p) => makeSet(p.seed, p.density));

  const paint = React.useCallback(() => {
    const cv = canvas.current, hold = holder.current;
    if (!cv || !hold) return;
    const ctx = cv.getContext("2d");
    if (!ctx) return;
    const gap = 1;
    const avail = Math.max(120, hold.clientWidth - 18);
    const cell = Math.max(2, Math.min(9, Math.floor((avail - (COLS - 1) * gap) / COLS)));
    const w = COLS * (cell + gap) - gap, h = ROWS * (cell + gap) - gap;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    cv.width = Math.round(w * dpr); cv.height = Math.round(h * dpr);
    cv.style.width = `${w}px`; cv.style.height = `${h}px`;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    const cs = getComputedStyle(document.documentElement);
    const setC = `hsl(${cs.getPropertyValue("--accent").trim()})`;
    const offC = `hsl(${cs.getPropertyValue("--unset").trim()})`;
    ctx.clearRect(0, 0, w, h);
    for (let i = 0; i < COLS * ROWS; i++) {
      const bit = (out.current[i >>> 5] >>> (i & 31)) & 1;
      ctx.fillStyle = bit ? setC : offC;
      ctx.fillRect((i % COLS) * (cell + gap), ((i / COLS) | 0) * (cell + gap), cell, cell);
    }
  }, []);

  const run = React.useCallback(() => {
    const active = sets.current!.filter((_, i) => on[i]);
    const o = out.current;
    if (!active.length) { o.fill(0xffffffff); return BITS; }
    let c = 0;
    for (let i = 0; i < WORDS; i++) {
      let v = active[0][i];
      for (let k = 1; k < active.length; k++) v &= active[k][i];
      o[i] = v;
      c += popcount(v);
    }
    return c;
  }, [on]);

  React.useEffect(() => {
    // performance.now() is deliberately coarsened in browsers, so timing a
    // single ~50 µs pass reads as zero. Run batches until enough wall clock has
    // accumulated to be above the clock's own resolution, then divide.
    const BUDGET_MS = 24;
    let count = 0, runs = 0, elapsed = 0;
    const t0 = performance.now();
    do {
      for (let t = 0; t < 32; t++) count = run();
      runs += 32;
      elapsed = performance.now() - t0;
    } while (elapsed < BUDGET_MS && runs < 4_096);

    setStats({
      count,
      us: (elapsed * 1000) / runs,
      words: WORDS * (on.filter(Boolean).length || 1),
      runs,
    });
    paint();
  }, [on, run, paint]);

  React.useEffect(() => {
    const onResize = () => paint();
    window.addEventListener("resize", onResize);
    const mo = new MutationObserver(() => paint());
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => { window.removeEventListener("resize", onResize); mo.disconnect(); };
  }, [paint]);

  return (
    <div className="overflow-hidden rounded-lg border border-line bg-surface">
      <div className="flex items-center gap-2.5 border-b border-line bg-surface-raised px-3 py-2 font-mono text-xs text-fg-faint">
        <span className="size-1.5 rounded-full bg-accent shadow-[0_0_0_3px_hsl(var(--accent)/0.15)]" aria-hidden />
        POST /table/events/query
      </div>

      <div className="p-4">
        <pre className="whitespace-pre-wrap break-words font-mono text-base leading-[1.75] text-fg-muted">
          <span className="text-fg-faint">Count(</span>{"\n  "}
          <span className="text-fg-faint">Intersect(</span>
          {PREDICATES.map((p, i) => (
            <React.Fragment key={p.label}>
              {"\n    "}
              <span className={cn(on[i] ? "text-accent" : "text-fg-faint line-through decoration-1 opacity-60")}>
                Row({p.label})
              </span>
              {i < PREDICATES.length - 1 && <span className="text-fg-faint">,</span>}
            </React.Fragment>
          ))}
          {"\n  "}<span className="text-fg-faint">)</span>{"\n"}
          <span className="text-fg-faint">)</span>
        </pre>

        <div role="group" aria-label="Toggle predicates" className="mt-3.5 flex flex-wrap gap-2">
          {PREDICATES.map((p, i) => (
            <button
              key={p.label}
              aria-pressed={on[i]}
              onClick={() => setOn((v) => v.map((x, k) => (k === i ? !x : x)))}
              className={cn(
                "inline-flex items-center gap-2 rounded border px-2.5 py-1 font-mono text-xs transition-colors duration-fast",
                on[i] ? "border-accent-line bg-accent-soft text-fg" : "border-line-strong text-fg-muted hover:text-fg",
              )}
            >
              <span aria-hidden className={cn("block size-[7px] rounded-[1px]", on[i] ? "bg-accent" : "bg-unset")} />
              {p.label}
            </button>
          ))}
        </div>

        <div ref={holder} className="mt-4 rounded border border-line bg-surface-raised p-2">
          <canvas ref={canvas} className="mx-auto block"
            aria-label={`Bit matrix: ${num(stats.count)} of ${num(BITS)} records match the current predicates`} />
        </div>
      </div>

      <div className="grid grid-cols-2 border-t border-line sm:grid-cols-3">
        <Readout k="matching records" v={num(stats.count)} />
        <Readout k="query time" v={<><span className="text-accent">{stats.us.toFixed(stats.us < 100 ? 1 : 0)}</span> µs</>} />
        <Readout k="words touched" v={num(stats.words)} className="col-span-2 border-t sm:col-span-1 sm:border-t-0" />
      </div>

      <p className="border-t border-line px-4 py-2.5 text-xs leading-relaxed text-fg-faint">
        Measured in your browser, now. Each predicate is a real 1,048,576-bit set (32,768 × 32-bit words);
        the grid draws the first 2,048 records, one cell per record. The time is the mean over{" "}
        {num(stats.runs)} runs — one pass is faster than this browser&rsquo;s clock can resolve, so it is
        timed in batches. Nothing is scanned, decoded or materialised: the loop ANDs machine words and
        counts the ones that stayed.
      </p>
    </div>
  );
}

function Readout({ k, v, className }: { k: string; v: React.ReactNode; className?: string }) {
  return (
    <div className={cn("border-r border-line px-4 py-3 last:border-r-0", className)}>
      <span className="mb-0.5 block text-xs text-fg-faint">{k}</span>
      <span className="font-mono text-lg tabular text-fg">{v}</span>
    </div>
  );
}
