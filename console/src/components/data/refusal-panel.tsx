"use client";
import { ArrowRight, Ban, BookOpen, Wand2 } from "lucide-react";
import { ApiError } from "@/lib/api/types";
import { lookupRefusal } from "@/lib/refusals";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/badges/status-badge";
import { cn } from "@/lib/cn";

/**
 * A refusal is the product, not a failure of it.
 *
 * The planner rejects unsupported constructs *by name, at parse time*. This
 * panel is what that looks like: the construct named in the headline, the exact
 * text underlined in place, the stable code you can grep your logs for, one
 * sentence of reason, and the thing that exists instead — with a one-click
 * rewrite whenever the alternative is mechanical.
 *
 * It is deliberately violet, not red. Nothing broke. The engine told you what
 * it is, and told you early.
 */
export function RefusalPanel({ error, query, onRewrite, className }: {
  error: ApiError; query: string; onRewrite?: (next: string) => void; className?: string;
}) {
  const spec = lookupRefusal(error.code, error.message);
  const construct = spec?.construct ?? error.code.replace(/_/g, " ");
  const rewritten = spec?.rewrite && query ? spec.rewrite(query) : null;

  const excerpt = error.span ? query.slice(error.span[0], error.span[1]) : null;
  const before = error.span ? query.slice(Math.max(0, error.span[0] - 42), error.span[0]) : "";
  const after = error.span ? query.slice(error.span[1], error.span[1] + 42) : "";

  return (
    <div
      role="status"
      aria-live="polite"
      className={cn("overflow-hidden rounded-lg border border-refused/40 bg-refused-soft/40", className)}
    >
      <div className="flex items-start gap-3 border-b border-refused/25 px-4 py-3">
        <span className="mt-0.5 flex size-6 shrink-0 items-center justify-center rounded-sm bg-refused/15 text-refused">
          <Ban className="size-3.5" aria-hidden />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="text-md font-semibold text-fg">
              <span className="text-refused">Refused</span>
              <span className="text-fg-faint"> · </span>
              <span className="font-mono">{construct}</span>
            </h3>
            <Badge tone="refused">{error.code}</Badge>
            <Badge tone="neutral">HTTP {error.status}</Badge>
          </div>
          <p className="mt-1.5 text-base leading-relaxed text-fg-muted">
            {spec?.reason ?? error.message}
          </p>
        </div>
      </div>

      {excerpt && (
        <div className="border-b border-refused/20 px-4 py-3">
          <div className="mb-1.5 text-2xs font-medium uppercase tracking-wider text-fg-faint">
            refused at byte {error.span![0]}–{error.span![1]}
          </div>
          <pre className="overflow-x-auto rounded border border-line bg-surface-sunken px-2.5 py-2 text-sm leading-relaxed">
            <code>
              <span className="text-fg-faint">{before.replace(/\n/g, " ")}</span>
              <span className="rounded-sm bg-refused/22 px-0.5 text-fg underline decoration-refused decoration-wavy decoration-from-font underline-offset-4">
                {excerpt}
              </span>
              <span className="text-fg-faint">{after.replace(/\n/g, " ")}</span>
            </code>
          </pre>
        </div>
      )}

      {spec && (
        <div className="px-4 py-3">
          <div className="mb-1.5 flex items-center gap-1.5 text-2xs font-medium uppercase tracking-wider text-fg-faint">
            <ArrowRight className="size-3" aria-hidden />
            what exists instead
          </div>
          <p className="text-base leading-relaxed text-fg">{spec.instead}</p>

          {rewritten && onRewrite && (
            <div className="mt-3 rounded border border-line bg-surface">
              <div className="flex items-center justify-between gap-3 border-b border-line px-2.5 py-1.5">
                <span className="text-2xs font-medium uppercase tracking-wider text-fg-faint">suggested rewrite</span>
                <Button size="xs" variant="primary" onClick={() => onRewrite(rewritten)}>
                  <Wand2 aria-hidden /> Apply
                </Button>
              </div>
              <pre className="overflow-x-auto px-2.5 py-2 text-sm leading-relaxed text-fg"><code>{rewritten}</code></pre>
            </div>
          )}

          <div className="mt-3 flex items-center gap-1.5 text-xs text-fg-faint">
            <BookOpen className="size-3" aria-hidden />
            <span>The server's own words: </span>
            <code className="text-fg-muted">{error.message}</code>
          </div>
        </div>
      )}
    </div>
  );
}

/** The same idea, one line high — for a results toolbar or a log row. */
export function RefusalLine({ code, message }: { code: string; message: string }) {
  const spec = lookupRefusal(code, message);
  return (
    <span className="inline-flex items-center gap-2 text-sm">
      <Badge tone="refused"><Ban className="size-2.5" aria-hidden /> refused</Badge>
      <span className="font-mono text-xs text-refused">{code}</span>
      <span className="truncate text-fg-muted">{spec?.instead ?? message}</span>
    </span>
  );
}
