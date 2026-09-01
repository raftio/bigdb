"use client";
import { cn } from "@/lib/cn";
import { Tooltip } from "@/components/ui/tooltip";
import { compact, ms, num, bytes } from "@/lib/format";
import type { Metrics } from "@/lib/api/types";

/**
 * The shape of the request path, with each layer showing its own number.
 *
 * This is a diagram because the architecture *is* a stack: a request enters at
 * the edge, is routed to the copy that owns the range, is parsed by the facade,
 * planned once for both surfaces, answered over bitmaps, and lands on pages.
 * Reading top to bottom tells you where the time and the bytes went.
 */
export function ArchitectureDiagram({ metrics, cluster, className }: {
  metrics: Metrics; cluster: boolean; className?: string;
}) {
  const layers = [
    {
      id: "edge", name: "Edge", detail: "HTTP/1.1 · TLS at the proxy",
      stats: [
        { label: "accepted", value: compact(metrics.big_http_connections_accepted_total), title: "big_http_connections_accepted_total" },
        { label: "rejected", value: num(metrics.big_http_connections_rejected_total), tone: metrics.big_http_connections_rejected_total > 0 ? "degraded" : undefined, title: "big_http_connections_rejected_total" },
        { label: "401", value: num(metrics.big_http_unauthorized_total), title: "big_http_unauthorized_total" },
      ],
    },
    {
      id: "cluster", name: cluster ? "Cluster" : "Single node", detail: cluster ? "ranges from config · one copy serves a range · CP" : "one process, one file",
      stats: cluster
        ? [{ label: "failover", value: "≈1.0 s" }, { label: "schema leader", value: "big-01" }, { label: "stale reads", value: "never" }]
        : [{ label: "tenant", value: "1" }, { label: "file", value: "1" }],
    },
    {
      id: "facade", name: "Facade", detail: "auth · deadline · routing",
      stats: [
        { label: "requests", value: compact(metrics.big_http_requests_total), title: "big_http_requests_total" },
        { label: "timed out", value: num(metrics.big_http_queries_timed_out_total), tone: "degraded", title: "big_http_queries_timed_out_total" },
        { label: "cancelled", value: num(metrics.big_http_queries_cancelled_total), title: "big_http_queries_cancelled_total" },
      ],
    },
    {
      id: "planner", name: "Planner", detail: "one planner, two surfaces: SQL and PQL",
      stats: [
        { label: "2xx", value: compact(metrics.big_http_responses_total["2xx"]), tone: "healthy" },
        { label: "4xx", value: compact(metrics.big_http_responses_total["4xx"]), tone: "refused" },
        { label: "5xx", value: num(metrics.big_http_responses_total["5xx"]), tone: "failed" },
      ],
    },
    {
      id: "data", name: "Data", detail: "bitmap intersection · popcount · BSI arithmetic",
      stats: [
        { label: "p50", value: ms(metrics.latency.p50_ms) },
        { label: "p95", value: ms(metrics.latency.p95_ms) },
        { label: "p99", value: ms(metrics.latency.p99_ms) },
      ],
    },
    {
      id: "storage", name: "Storage", detail: "no WAL · pages → fsync → meta flip → fsync",
      stats: [
        { label: "pages", value: compact(metrics.big_page_count), title: "big_page_count" },
        { label: "reusable", value: compact(metrics.big_free_pages_reusable), title: "big_free_pages_reusable" },
        { label: "pending", value: compact(metrics.big_pages_pending_reclaim_reader + metrics.big_pages_pending_reclaim_retention), tone: "degraded", title: "big_pages_pending_reclaim_reader + _retention" },
      ],
    },
  ];

  return (
    <div className={cn("space-y-0", className)}>
      {layers.map((l, i) => (
        <div key={l.id}>
          <div className="group grid grid-cols-[132px_1fr] items-center gap-3 rounded border border-line bg-surface px-3 py-2 transition-colors duration-fast hover:border-line-strong">
            <div className="min-w-0">
              <div className="truncate text-base font-medium text-fg">{l.name}</div>
              <div className="truncate text-2xs text-fg-faint" title={l.detail}>{l.detail}</div>
            </div>
            <div className="flex flex-wrap items-center justify-end gap-x-5 gap-y-1">
              {l.stats.map((s) => (
                <Tooltip key={s.label} content={"title" in s && s.title ? <code className="text-2xs">{s.title}</code> : null}>
                  <div className="text-right">
                    <div className="text-2xs uppercase tracking-wider text-fg-faint">{s.label}</div>
                    <div className={cn("font-mono text-base leading-tight",
                      (s as any).tone === "healthy" ? "text-healthy" :
                      (s as any).tone === "degraded" ? "text-degraded" :
                      (s as any).tone === "refused" ? "text-refused" :
                      (s as any).tone === "failed" ? "text-failed" : "text-fg")}>
                      {s.value}
                    </div>
                  </div>
                </Tooltip>
              ))}
            </div>
          </div>
          {i < layers.length - 1 && (
            <div className="flex h-3 items-center justify-center" aria-hidden>
              <svg width="9" height="12" viewBox="0 0 9 12"><path d="M4.5 0v8M1 7l3.5 4L8 7" fill="none" stroke="hsl(var(--border-strong))" strokeWidth="1.2" /></svg>
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

/**
 * The row-key dictionary: the one allocation that grows with cardinality, and
 * therefore the one number that predicts when this deployment needs a bigger
 * machine. Shown as a gauge because a gauge is what "how close to the ceiling"
 * looks like — with the explanation next to it, not in a docs page.
 */
export function DictionaryGauge({ metrics, ceilingBytes, className }: {
  metrics: Metrics; ceilingBytes: number; className?: string;
}) {
  const used = metrics.row_key_dictionary_bytes / ceilingBytes;
  const tone = used > 0.85 ? "failed" : used > 0.6 ? "degraded" : "accent";
  const bar = { failed: "bg-failed", degraded: "bg-degraded", accent: "bg-accent" }[tone];

  return (
    <div className={cn("rounded-lg border border-line bg-surface p-3", className)}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs font-medium uppercase tracking-wide text-fg-faint">row-key dictionary</span>
        <span className="font-mono text-2xs text-fg-faint">{num(metrics.row_key_dictionary_keys)} keys</span>
      </div>
      <div className="mt-1.5 flex items-baseline gap-1.5">
        <span className={cn("font-mono text-xl leading-none", tone === "accent" ? "text-fg" : `text-${tone}`)}>
          {bytes(metrics.row_key_dictionary_bytes, 1)}
        </span>
        <span className="font-mono text-xs text-fg-faint">of {bytes(ceilingBytes, 0)} resident</span>
      </div>
      <div className="mt-2 h-1.5 w-full overflow-hidden rounded-sm bg-surface-sunken" role="img"
        aria-label={`Dictionary at ${Math.round(used * 100)} percent of resident memory`}>
        <div className={cn("h-full rounded-sm transition-[width] duration-150", bar)} style={{ width: `${Math.min(100, used * 100)}%` }} />
      </div>
      <p className="mt-2 text-xs leading-relaxed text-fg-muted">
        Every distinct value of a keyed field is interned once, forever. Bitmaps stay flat as records grow; this does
        not. It is the allocation to watch — and the reason a high-cardinality field costs more than a wide one.
      </p>
    </div>
  );
}
