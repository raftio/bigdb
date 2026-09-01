"use client";
import { cn } from "@/lib/cn";
import { Tooltip } from "@/components/ui/tooltip";
import { num, us } from "@/lib/format";
import type { QueryTiming } from "@/lib/api/types";

/**
 * Where the time went: parse, plan, the fan-out to each shard, and the merge.
 *
 * The fan-out is the interesting part, so each shard gets its own bar on its own
 * line. Shards run concurrently, so the wall-clock cost of the fan-out is the
 * slowest one -- which is exactly what the strip makes visible: the widest bar
 * is what you would have to fix.
 */
export function TimingStrip({ timing, className }: { timing: QueryTiming; className?: string }) {
  const slowest = Math.max(...timing.shards.map((s) => s.us), 1);
  const stages: Array<{ label: string; us: number; color: string }> = [
    { label: "parse", us: timing.parse_us, color: "bg-viz-p99" },
    { label: "plan", us: timing.plan_us, color: "bg-viz-p95" },
    { label: "fan-out", us: slowest, color: "bg-accent" },
    { label: "merge", us: timing.merge_us, color: "bg-viz-p50" },
  ];
  const total = stages.reduce((a, s) => a + s.us, 0) || 1;

  return (
    <div className={cn("space-y-2", className)}>
      <div className="flex items-baseline gap-2">
        <span className="text-2xs uppercase tracking-wider text-fg-faint">timing</span>
        <span className="ml-auto font-mono text-sm text-fg">{us(timing.total_us)}</span>
      </div>

      <div className="flex h-2 w-full gap-px overflow-hidden rounded-sm" role="img"
        aria-label={stages.map((s) => `${s.label} ${us(s.us)}`).join(", ")}>
        {stages.map((s) => (
          <Tooltip key={s.label} content={<span className="font-mono text-2xs">{s.label} {us(s.us)}</span>}>
            <div className={cn(s.color, "opacity-85")} style={{ width: `${(s.us / total) * 100}%` }} />
          </Tooltip>
        ))}
      </div>

      <div className="flex flex-wrap gap-x-4 gap-y-0.5 font-mono text-2xs">
        {stages.map((s) => (
          <span key={s.label} className="text-fg-faint">
            {s.label} <span className="text-fg-muted">{us(s.us)}</span>
          </span>
        ))}
      </div>

      <div className="space-y-px pt-1">
        <div className="text-2xs uppercase tracking-wider text-fg-faint">
          fan-out &middot; {timing.shards.length} shards, concurrent
        </div>
        {timing.shards.map((s) => (
          <div key={s.shard} className="grid grid-cols-[38px_46px_1fr_62px] items-center gap-1.5">
            <span className="font-mono text-2xs text-fg-muted">{s.shard}</span>
            <span className="font-mono text-2xs text-fg-faint">{s.node}</span>
            <span className="h-1.5 overflow-hidden rounded-sm bg-surface-sunken">
              <span className={cn("block h-full rounded-sm", s.us === slowest ? "bg-degraded" : "bg-accent/60")}
                style={{ width: `${(s.us / slowest) * 100}%` }} />
            </span>
            <Tooltip content={<span className="font-mono text-2xs">{num(s.rows_touched)} records touched</span>}>
              <span className="text-right font-mono text-2xs tabular text-fg-muted">{us(s.us)}</span>
            </Tooltip>
          </div>
        ))}
      </div>
    </div>
  );
}
