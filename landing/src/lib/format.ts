/** Numbers are the hero. Every formatter here is tabular-safe and lossless-first. */

const nf = new Intl.NumberFormat("en-US");

export const num = (n: number) => nf.format(Math.round(n));

/** Compact only where the exact value is one hover away. */
export function compact(n: number, digits = 1): string {
  const a = Math.abs(n);
  if (a < 1_000) return String(Math.round(n));
  const units: Array<[number, string]> = [[1e12, "T"], [1e9, "B"], [1e6, "M"], [1e3, "k"]];
  for (const [v, s] of units) if (a >= v) return `${(n / v).toFixed(digits).replace(/\.0$/, "")}${s}`;
  return String(n);
}

/** Binary, because pages and files are binary. */
export function bytes(n: number, digits = 1): string {
  if (n < 1024) return `${n} B`;
  const u = ["KiB", "MiB", "GiB", "TiB", "PiB"];
  let v = n / 1024, i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(digits)} ${u[i]}`;
}

/** Latency is never rounded to a lie: sub-millisecond keeps its digits. */
export function ms(v: number): string {
  if (v < 1) return `${v.toFixed(2)} ms`;
  if (v < 100) return `${v.toFixed(1)} ms`;
  return `${Math.round(v)} ms`;
}

export function us(v: number): string {
  if (v < 1_000) return `${v} µs`;
  return `${(v / 1_000).toFixed(v < 10_000 ? 2 : 1)} ms`;
}

export function duration(msv: number): string {
  if (msv < 1_000) return `${Math.round(msv)} ms`;
  const s = msv / 1_000;
  if (s < 60) return `${s.toFixed(1)} s`;
  const m = Math.floor(s / 60);
  return `${m}m ${Math.round(s - m * 60)}s`;
}

export function pct(n: number, digits = 1): string {
  return `${(n * 100).toFixed(digits)}%`;
}

export function relTime(iso: string | null): string {
  if (!iso) return "never";
  const d = Date.now() - new Date(iso).getTime();
  // A timestamp ahead of this clock means skew, not a negative age.
  if (d < 0) return "just now";
  const s = Math.round(d / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}

export function absTime(iso: string): string {
  return new Date(iso).toISOString().replace("T", " ").replace(/\.\d+Z$/, "Z");
}

export function clock(iso: string): string {
  return new Date(iso).toISOString().slice(11, 23);
}

/** A decimal field stores an integer; the scale is what makes it a price. */
export function scaled(v: number, scale?: number): string {
  return scale ? v.toFixed(scale) : num(v);
}
