"use client";
import * as React from "react";
import { cn } from "@/lib/cn";
import type { LatencySeries, Series } from "@/lib/api/types";
import { ms } from "@/lib/format";

/**
 * Small multiples over big charts. A sparkline is one SVG path, no axes, no
 * legend — the number above it is the reading, this is only the shape.
 */
export function Sparkline({ data, className, stroke = "hsl(var(--viz-fill))", fill = true }: {
  data: Series[]; className?: string; stroke?: string; fill?: boolean;
}) {
  const { d, area } = React.useMemo(() => {
    if (data.length < 2) return { d: "", area: "" };
    const xs = data.map((_, i) => (i / (data.length - 1)) * 100);
    const lo = Math.min(...data.map((p) => p.v));
    const hi = Math.max(...data.map((p) => p.v));
    const span = hi - lo || 1;
    const ys = data.map((p) => 100 - ((p.v - lo) / span) * 92 - 4);
    const path = xs.map((x, i) => `${i ? "L" : "M"}${x.toFixed(2)} ${ys[i].toFixed(2)}`).join(" ");
    return { d: path, area: `${path} L100 100 L0 100 Z` };
  }, [data]);

  return (
    <svg viewBox="0 0 100 100" preserveAspectRatio="none" className={cn("overflow-visible", className)} aria-hidden>
      {fill && <path d={area} fill={stroke} opacity={0.1} />}
      <path d={d} fill="none" stroke={stroke} strokeWidth={1.4} vectorEffect="non-scaling-stroke"
        strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  );
}

/**
 * Latency, honestly: p50 as a line inside a p95–p99 band. Never a mean — a mean
 * over a heavy tail describes a request nobody made.
 */
export function LatencyBand({ data, className, showLegend = true }: {
  data: LatencySeries[]; className?: string; showLegend?: boolean;
}) {
  const geom = React.useMemo(() => {
    if (data.length < 2) return null;
    const hi = Math.max(...data.map((p) => p.p99)) * 1.05;
    const x = (i: number) => (i / (data.length - 1)) * 100;
    const y = (v: number) => 100 - (v / hi) * 96 - 2;
    const line = (k: "p50" | "p95" | "p99") =>
      data.map((p, i) => `${i ? "L" : "M"}${x(i).toFixed(2)} ${y(p[k]).toFixed(2)}`).join(" ");
    const band =
      data.map((p, i) => `${i ? "L" : "M"}${x(i).toFixed(2)} ${y(p.p99).toFixed(2)}`).join(" ") +
      " " +
      [...data].reverse().map((p, i) => `L${x(data.length - 1 - i).toFixed(2)} ${y(p.p95).toFixed(2)}`).join(" ") + " Z";
    return { band, p50: line("p50"), p95: line("p95"), p99: line("p99"), hi };
  }, [data]);

  const last = data[data.length - 1];

  return (
    <div className={cn("flex flex-col gap-1.5", className)}>
      <svg viewBox="0 0 100 100" preserveAspectRatio="none" className="h-full min-h-0 w-full flex-1" role="img"
        aria-label={`Request latency percentiles. p50 ${ms(last?.p50 ?? 0)}, p95 ${ms(last?.p95 ?? 0)}, p99 ${ms(last?.p99 ?? 0)}.`}>
        {geom && (
          <>
            <path d={geom.band} fill="hsl(var(--viz-p99))" opacity={0.16} />
            <path d={geom.p99} fill="none" stroke="hsl(var(--viz-p99))" strokeWidth={1} strokeDasharray="3 2" vectorEffect="non-scaling-stroke" />
            <path d={geom.p95} fill="none" stroke="hsl(var(--viz-p95))" strokeWidth={1.2} vectorEffect="non-scaling-stroke" />
            <path d={geom.p50} fill="none" stroke="hsl(var(--viz-p50))" strokeWidth={1.6} vectorEffect="non-scaling-stroke" />
          </>
        )}
      </svg>
      {showLegend && last && (
        <div className="flex items-center gap-3 font-mono text-2xs">
          <Legend color="hsl(var(--viz-p50))" label="p50" value={ms(last.p50)} />
          <Legend color="hsl(var(--viz-p95))" label="p95" value={ms(last.p95)} />
          <Legend color="hsl(var(--viz-p99))" label="p99" value={ms(last.p99)} dashed />
          <span className="ml-auto text-fg-faint">derived from big_http_request_duration_seconds</span>
        </div>
      )}
    </div>
  );
}

function Legend({ color, label, value, dashed }: { color: string; label: string; value: string; dashed?: boolean }) {
  return (
    <span className="inline-flex items-center gap-1 text-fg-muted">
      <svg width="12" height="4" aria-hidden><line x1="0" y1="2" x2="12" y2="2" stroke={color} strokeWidth="1.6" strokeDasharray={dashed ? "3 2" : undefined} /></svg>
      <span className="text-fg-faint">{label}</span>
      <span className="text-fg">{value}</span>
    </span>
  );
}

/** A histogram as bars — used for the per-shard fan-out and page-class mixes. */
export function BarRow({ segments, className, height = 6 }: {
  segments: Array<{ label: string; value: number; color: string }>; className?: string; height?: number;
}) {
  const total = segments.reduce((a, s) => a + s.value, 0) || 1;
  return (
    <div className={cn("flex w-full gap-px overflow-hidden rounded-sm", className)} style={{ height }}
      role="img" aria-label={segments.map((s) => `${s.label} ${s.value}`).join(", ")}>
      {segments.map((s) => s.value > 0 && (
        <div key={s.label} className={s.color} style={{ width: `${(s.value / total) * 100}%` }} title={`${s.label}: ${s.value}`} />
      ))}
    </div>
  );
}
