import { cn } from "@/lib/cn";
import type { Health, NodeStatus } from "@/lib/api/types";
import { Tooltip } from "@/components/ui/tooltip";

/**
 * /health says the process is alive; /ready says it will answer. Both are shown,
 * because "up but not ready" is the state an operator most needs to see and the
 * one a single dot would hide. Never colour alone — the label always ships with it.
 */
export function healthOf(s: NodeStatus): Health {
  if (!s.live) return "failed";
  if (!s.ready) return "degraded";
  return "healthy";
}

const SPEC: Record<Health, { dot: string; text: string; label: string; glyph: string }> = {
  healthy:  { dot: "bg-healthy",  text: "text-healthy",  label: "healthy",  glyph: "●" },
  degraded: { dot: "bg-degraded", text: "text-degraded", label: "degraded", glyph: "◐" },
  failed:   { dot: "bg-failed",   text: "text-failed",   label: "down",     glyph: "○" },
  unknown:  { dot: "bg-unknown",  text: "text-unknown",  label: "unknown",  glyph: "◌" },
};

export function HealthDot({ status, showLabel = true, className }: {
  status: NodeStatus; showLabel?: boolean; className?: string;
}) {
  const h = healthOf(status);
  const s = SPEC[h];
  return (
    <Tooltip content={
      <div className="space-y-0.5 font-mono text-2xs">
        <div>GET /health → {status.live ? "200" : "no response"}</div>
        <div>GET /ready  → {status.live ? (status.ready ? "200" : "503") : "no response"}</div>
      </div>
    }>
      <span className={cn("inline-flex items-center gap-1.5", className)}>
        <span className="relative inline-flex size-1.5 shrink-0" aria-hidden>
          <span className={cn("absolute inset-0 rounded-full", s.dot, h === "degraded" && "animate-pulse-dot")} />
        </span>
        {showLabel && <span className={cn("text-xs font-medium", s.text)}>{s.label}</span>}
        <span className="sr-only">{s.label}</span>
      </span>
    </Tooltip>
  );
}
