"use client";
import { cn } from "@/lib/cn";
import { Tooltip } from "@/components/ui/tooltip";
import { Sparkline } from "./sparkline";
import type { Series } from "@/lib/api/types";

/**
 * The number is the hero. The label is small, the unit is quiet, and the
 * sparkline sits under the value rather than beside it so a row of tiles
 * reads as one continuous instrument.
 */
export function MetricTile({ label, value, unit, sub, series, tone = "default", metric, className, onClick }: {
  label: string;
  value: React.ReactNode;
  unit?: string;
  sub?: React.ReactNode;
  series?: Series[];
  tone?: "default" | "healthy" | "degraded" | "failed" | "accent";
  /** The Prometheus name behind this number, so it can be looked up. */
  metric?: string;
  className?: string;
  onClick?: () => void;
}) {
  const toneClass = {
    default: "text-fg", healthy: "text-healthy", degraded: "text-degraded",
    failed: "text-failed", accent: "text-accent",
  }[tone];

  const body = (
    <div className={cn(
      "group flex flex-col gap-1 rounded-lg border border-line bg-surface px-3 py-2.5",
      onClick && "cursor-pointer transition-colors duration-fast hover:border-line-strong",
      className,
    )}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="truncate text-xs font-medium uppercase tracking-wide text-fg-faint">{label}</span>
        {sub && <span className="shrink-0 font-mono text-2xs text-fg-faint">{sub}</span>}
      </div>
      <div className="flex items-baseline gap-1">
        <span className={cn("font-mono text-xl leading-none", toneClass)}>{value}</span>
        {unit && <span className="font-mono text-xs text-fg-faint">{unit}</span>}
      </div>
      {series && series.length > 1 && (
        <Sparkline data={series} className="mt-1 h-6 w-full" />
      )}
    </div>
  );

  return metric ? (
    <Tooltip content={<code className="text-2xs">{metric}</code>} side="bottom">{body}</Tooltip>
  ) : body;
}
