import { cn } from "@/lib/cn";
import type { TableEngine } from "@/lib/types";
import { Tooltip } from "@/components/ui/tooltip";

/**
 * The engine is fixed at CREATE and decides what the table writes for every
 * fact — so it travels with the table's name everywhere it is printed.
 * Shape carries the meaning as well as colour: ▮ bitmap, ▮▯ both, ▯ columnar.
 */
const SPEC: Record<TableEngine, { short: string; glyph: [boolean, boolean]; why: string; tone: string }> = {
  bitmap: {
    short: "bitmap", glyph: [true, false],
    tone: "border-accent/35 text-accent bg-accent-soft/60",
    why: "One bit per fact. Filters are bitmap intersections and counts are popcounts — no scan, and no stored value to read back.",
  },
  "bitmap+columnar": {
    short: "bitmap+col", glyph: [true, true],
    tone: "border-line-strong text-fg bg-surface-raised",
    why: "Writes both: the bitmap answers the filter, the column store answers the value. Costs both on write.",
  },
  columnar: {
    short: "columnar", glyph: [false, true],
    tone: "border-line-strong text-fg-muted bg-surface-sunken",
    why: "Values only. Aggregates read the column; a filter has no bitmap to intersect and must scan.",
  },
};

export function EngineBadge({ engine, size = "sm", className }: {
  engine: TableEngine; size?: "xs" | "sm"; className?: string;
}) {
  const s = SPEC[engine];
  return (
    <Tooltip content={<span><b className="font-mono">{engine}</b> — {s.why}</span>}>
      <span
        className={cn(
          "inline-flex shrink-0 items-center gap-1 rounded-sm border font-mono tracking-tight",
          size === "xs" ? "h-[15px] px-1 text-2xs" : "h-[18px] px-1.5 text-2xs",
          s.tone, className,
        )}
      >
        <EngineGlyph bitmap={s.glyph[0]} columnar={s.glyph[1]} />
        {s.short}
      </span>
    </Tooltip>
  );
}

/**
 * Drawn rather than typed: the box-drawing characters this wants are not in
 * every mono face, and a tofu box in a badge reads as a bug.
 */
function EngineGlyph({ bitmap, columnar }: { bitmap: boolean; columnar: boolean }) {
  return (
    <svg width="9" height="8" viewBox="0 0 9 8" aria-hidden className="shrink-0 opacity-80">
      {bitmap && <rect x="0.5" y="0.5" width="3" height="7" rx="0.5" fill="currentColor" />}
      {columnar && (
        <rect x={bitmap ? 4.5 : 0.5} y="0.5" width="3" height="7" rx="0.5"
          fill="none" stroke="currentColor" strokeWidth="1" />
      )}
    </svg>
  );
}

/** The mix across a whole deployment, as one dense bar. */
export function EngineMix({ engines, className }: { engines: Record<TableEngine, number>; className?: string }) {
  const total = Object.values(engines).reduce((a, b) => a + b, 0);
  if (!total) return <span className="text-xs text-fg-faint">no tables</span>;
  const order: TableEngine[] = ["bitmap", "bitmap+columnar", "columnar"];
  const fill: Record<TableEngine, string> = {
    bitmap: "bg-accent", "bitmap+columnar": "bg-accent/45", columnar: "bg-unset",
  };
  return (
    <Tooltip content={
      <div className="space-y-0.5 font-mono text-2xs">
        {order.map((e) => <div key={e}>{engines[e]} × {e}</div>)}
      </div>
    }>
      <div className={cn("flex h-1.5 w-24 gap-px overflow-hidden rounded-sm", className)} role="img"
        aria-label={order.map((e) => `${engines[e]} ${e}`).join(", ")}>
        {order.map((e) => engines[e] > 0 && (
          <div key={e} className={fill[e]} style={{ flexGrow: engines[e] }} />
        ))}
      </div>
    </Tooltip>
  );
}
