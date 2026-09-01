import { cn } from "@/lib/cn";

type Tone = "neutral" | "healthy" | "degraded" | "refused" | "failed" | "accent";

const TONE: Record<Tone, string> = {
  neutral:  "border-line-strong text-fg-muted bg-surface-sunken",
  healthy:  "border-healthy/40 text-healthy bg-healthy-soft",
  degraded: "border-degraded/45 text-degraded bg-degraded-soft",
  refused:  "border-refused/45 text-refused bg-refused-soft",
  failed:   "border-failed/45 text-failed bg-failed-soft",
  accent:   "border-accent/40 text-accent bg-accent-soft/60",
};

export function Badge({ tone = "neutral", mono = true, className, children }: {
  tone?: Tone; mono?: boolean; className?: string; children: React.ReactNode;
}) {
  return (
    <span className={cn(
      "inline-flex h-[18px] shrink-0 items-center gap-1 rounded-sm border px-1.5 text-2xs",
      mono && "font-mono", TONE[tone], className,
    )}>
      {children}
    </span>
  );
}

/** An HTTP status class, coloured by what it means to an operator. */
export function StatusBadge({ status, code }: { status: number; code?: string }) {
  const tone: Tone =
    status < 300 ? "healthy" :
    status === 499 ? "neutral" :
    status < 500 ? (code && code !== "parse_error" && !code.startsWith("un") ? "refused" : "degraded") :
    "failed";
  return (
    <span className="inline-flex items-center gap-1.5">
      <Badge tone={tone}>{status}</Badge>
      {code && <span className="font-mono text-2xs text-fg-faint">{code}</span>}
    </span>
  );
}
