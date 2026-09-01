"use client";
import { PlugZap, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import type { Deployment } from "@/lib/api/types";

/**
 * When a deployment stops answering, every number on the screen is stale. The
 * banner says so once, at the top, rather than letting eight tiles each show a
 * separate error — and it says exactly which probe failed.
 */
export function ConnectionBanner({ deployment, onRetry }: { deployment: Deployment; onRetry?: () => void }) {
  if (deployment.status.live && deployment.status.ready) return null;
  const down = !deployment.status.live;

  return (
    <div
      role="alert"
      className={`flex flex-wrap items-center gap-x-3 gap-y-1 border-b px-4 py-2 text-base ${
        down ? "border-failed/35 bg-failed-soft/60" : "border-degraded/35 bg-degraded-soft/60"
      }`}
    >
      <PlugZap className={`size-3.5 shrink-0 ${down ? "text-failed" : "text-degraded"}`} aria-hidden />
      <span className="font-medium text-fg">
        {down ? "This deployment is not answering." : "This deployment is up but not ready."}
      </span>
      <span className="text-fg-muted">
        {down ? (
          <><code>GET /health</code> to <code>{deployment.host}</code> returned no response. Everything below is the last value the control plane saw.</>
        ) : (
          <><code>GET /health</code> is 200 but <code>GET /ready</code> is 503 — the process is alive and will refuse queries until it is not.</>
        )}
      </span>
      {onRetry && (
        <Button size="xs" variant="outline" className="ml-auto" onClick={onRetry}>
          <RefreshCw aria-hidden /> Re-probe
        </Button>
      )}
    </div>
  );
}
