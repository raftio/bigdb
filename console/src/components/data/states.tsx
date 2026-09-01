"use client";
import { AlertTriangle, Inbox, Lock, PlugZap, RefreshCw } from "lucide-react";
import { ApiError, type Role } from "@/lib/api/types";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/badges/status-badge";
import { SkeletonRows } from "@/components/ui/skeleton";
import { cn } from "@/lib/cn";

/**
 * Every data-bearing component in this console resolves to exactly one of:
 * loading · empty · error · refused · unauthorized. They are all here, so none
 * of them is improvised at the call site.
 */

export function LoadingState({ rows = 6, label = "Loading" }: { rows?: number; label?: string }) {
  return <SkeletonRows rows={rows} className="min-h-[120px]" />;
}

export function EmptyState({ title, description, action, icon: Icon = Inbox, className }: {
  title: string; description?: React.ReactNode; action?: React.ReactNode;
  icon?: React.ComponentType<{ className?: string }>; className?: string;
}) {
  return (
    <div className={cn("flex flex-col items-center justify-center gap-2 px-6 py-12 text-center", className)}>
      <Icon className="size-5 text-fg-faint" aria-hidden />
      <h3 className="text-md font-medium text-fg">{title}</h3>
      {description && <p className="max-w-md text-base leading-relaxed text-fg-muted">{description}</p>}
      {action && <div className="mt-2">{action}</div>}
    </div>
  );
}

export function UnauthorizedState({ have, need, what }: { have: Role; need: Role; what: string }) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 px-6 py-12 text-center">
      <Lock className="size-5 text-fg-faint" aria-hidden />
      <h3 className="text-md font-medium text-fg">{what} needs the <code className="text-degraded">{need}</code> role</h3>
      <p className="max-w-md text-base leading-relaxed text-fg-muted">
        The token this console holds for this deployment is <code>{have}</code>. Roles are verbs, not rows —
        ask an admin for a <code>{need}</code> token, or switch tokens in <b>Access</b>.
      </p>
      <Badge tone="degraded" className="mt-1">HTTP 403 · forbidden</Badge>
    </div>
  );
}

/**
 * A genuine failure: the request did not produce an answer. Distinct in colour
 * and in wording from a refusal, which did.
 */
export function ErrorState({ error, onRetry, className }: {
  error: unknown; onRetry?: () => void; className?: string;
}) {
  const api = error instanceof ApiError ? error : null;
  const unreachable = api?.status === 503 || api?.code === "unreachable";
  const timedOut = api?.status === 504;

  return (
    <div className={cn("flex flex-col items-center justify-center gap-2 px-6 py-10 text-center", className)}>
      {unreachable ? <PlugZap className="size-5 text-failed" aria-hidden /> : <AlertTriangle className="size-5 text-failed" aria-hidden />}
      <h3 className="text-md font-medium text-fg">
        {unreachable ? "This deployment did not answer"
          : timedOut ? "The deadline expired before the answer did"
          : "The request failed"}
      </h3>
      <p className="max-w-md text-base leading-relaxed text-fg-muted">
        {api ? api.message : String(error)}
      </p>
      {api?.shards?.length ? (
        <p className="max-w-md text-sm text-fg-muted">
          The owner that could not answer holds{" "}
          <code>{api.shards.join(", ")}</code>. Every other shard answered; this result would have been partial,
          so it was refused instead of returned incomplete.
        </p>
      ) : null}
      <div className="mt-1 flex items-center gap-2">
        {api && <Badge tone="failed">HTTP {api.status} · {api.code}</Badge>}
        {onRetry && <Button size="xs" variant="outline" onClick={onRetry}><RefreshCw aria-hidden /> Retry</Button>}
      </div>
    </div>
  );
}

/** One line, for a panel whose failure is already explained above it. */
export function QuietError({ note = "no data: the deployment did not answer" }: { note?: string }) {
  return (
    <p className="px-3 py-4 text-center text-sm text-fg-faint" role="status">{note}</p>
  );
}

/** Wraps the five states around a query result so screens stay declarative. */
export function QueryBoundary<T>({ query, children, empty, loadingRows, unauthorized, errorFallback }: {
  query: { data?: T; isLoading: boolean; error: unknown; refetch?: () => void };
  children: (data: T) => React.ReactNode;
  empty?: (data: T) => boolean;
  loadingRows?: number;
  unauthorized?: { have: Role; need: Role; what: string };
  /** Use when the same failure is already reported once on this screen. */
  errorFallback?: React.ReactNode;
}) {
  if (query.isLoading) return <LoadingState rows={loadingRows} />;
  if (query.error) {
    const api = query.error instanceof ApiError ? query.error : null;
    if (api?.status === 403 && unauthorized) return <UnauthorizedState {...unauthorized} />;
    if (errorFallback !== undefined) return <>{errorFallback}</>;
    return <ErrorState error={query.error} onRetry={query.refetch} />;
  }
  if (query.data === undefined) return <LoadingState rows={loadingRows} />;
  if (empty?.(query.data)) return <EmptyState title="Nothing here yet" />;
  return <>{children(query.data)}</>;
}
