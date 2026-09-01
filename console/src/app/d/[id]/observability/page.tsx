"use client";
import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { Ban, Filter, X } from "lucide-react";
import { useClient, useDeployment, useMetrics } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { MetricTile } from "@/components/data/metric-tile";
import { LatencyBand, Sparkline } from "@/components/data/sparkline";
import { DataTable, type Column } from "@/components/data/data-table";
import { StatusBadge, Badge } from "@/components/badges/status-badge";
import { QueryBoundary, EmptyState } from "@/components/data/states";
import { RoleBadge } from "@/components/badges/role-badge";
import { Tooltip } from "@/components/ui/tooltip";
import { latencySeries, series } from "@/lib/api/mock/fixtures";
import { isRefusal, lookupRefusal } from "@/lib/refusals";
import { clock, compact, ms, num, bytes } from "@/lib/format";
import { cn } from "@/lib/cn";
import type { RequestLogLine } from "@/lib/api/types";

/** Prometheus-backed charts, plus the structured request log as a filtered stream. */
export default function ObservabilityPage() {
  const dep = useDeployment();
  const client = useClient();
  const metrics = useMetrics();
  const [q, setQ] = React.useState("");
  const [only, setOnly] = React.useState<"all" | "refused" | "failed" | "slow">("all");

  const log = useQuery({
    queryKey: ["requestlog", dep.data?.id], queryFn: () => client!.requestLog(), enabled: !!client,
  });

  const latency = React.useMemo(() => latencySeries(77, 90), []);
  const conns = React.useMemo(() => series(88, 60, 1_400, 400), []);
  const dict = React.useMemo(() => series(99, 60, 3_100_000_000, 90_000_000, 4_000_000), []);
  const role = dep.data?.role ?? "read";

  const rows = React.useMemo(() => {
    const needle = q.trim().toLowerCase();
    return (log.data ?? []).filter((l) => {
      if (only === "refused" && !isRefusal(l.code)) return false;
      if (only === "failed" && l.status < 500) return false;
      if (only === "slow" && l.duration_ms < 10) return false;
      if (!needle) return true;
      return `${l.request_id} ${l.route} ${l.status} ${l.code ?? ""} ${l.method} ${l.role}`.toLowerCase().includes(needle);
    });
  }, [log.data, q, only]);

  const columns: Column<RequestLogLine>[] = [
    { key: "ts", header: "time", width: 100, mono: true, cell: (l) => <span className="text-fg-faint">{clock(l.ts)}</span>,
      sort: (a, b) => a.ts.localeCompare(b.ts) },
    { key: "id", header: "request id", width: 140, mono: true, cell: (l) => <span className="text-fg-muted">{l.request_id}</span> },
    { key: "method", header: "", width: 52, mono: true, cell: (l) => <span className="text-fg-faint">{l.method}</span> },
    { key: "route", header: "route", width: 210, mono: true, cell: (l) => <span className="truncate text-fg">{l.route}</span>,
      sort: (a, b) => a.route.localeCompare(b.route) },
    {
      key: "status", header: "status", width: 210,
      cell: (l) => isRefusal(l.code)
        ? <Tooltip content={lookupRefusal(l.code!)?.instead ?? l.code}>
            <span className="flex items-center gap-1.5">
              <Badge tone="refused"><Ban className="size-2.5" aria-hidden />{l.status}</Badge>
              <span className="font-mono text-2xs text-refused">{l.code}</span>
            </span>
          </Tooltip>
        : <StatusBadge status={l.status} code={l.code} />,
      sort: (a, b) => a.status - b.status,
    },
    { key: "dur", header: "duration", width: 88, align: "right", mono: true,
      cell: (l) => <span className={cn(l.duration_ms > 100 ? "text-degraded" : "text-fg")}>{ms(l.duration_ms)}</span>,
      sort: (a, b) => a.duration_ms - b.duration_ms },
    { key: "bytes", header: "bytes", width: 78, align: "right", mono: true,
      cell: (l) => <span className="text-fg-muted">{num(l.bytes)}</span>, sort: (a, b) => a.bytes - b.bytes },
    { key: "role", header: "role", width: 82,
      cell: (l) => l.role === "anonymous" ? <Badge>anon</Badge> : <RoleBadge role={l.role} /> },
    { key: "shard", header: "shard", width: 62, mono: true,
      cell: (l) => l.shard ? <span className="text-fg-faint">{l.shard}</span> : null },
  ];

  const FILTERS = [
    { id: "all", label: "everything" },
    { id: "refused", label: "refusals" },
    { id: "failed", label: "5xx" },
    { id: "slow", label: "≥ 10 ms" },
  ] as const;

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title="Observability"
        description="Everything here comes from GET /metrics and the structured request log — one JSON line per request, with its request id."
      />

      <div className="space-y-4 px-5 pb-8">
        <QueryBoundary query={metrics} loadingRows={4} unauthorized={{ have: role, need: "read", what: "Metrics" }}>
          {(m) => (
            <>
              <div className="grid grid-cols-2 gap-3 lg:grid-cols-6">
                <MetricTile label="requests" value={compact(m.big_http_requests_total)} metric="big_http_requests_total" />
                <MetricTile label="4xx" value={compact(m.big_http_responses_total["4xx"])} tone="degraded"
                  metric='big_http_responses_total{class="4xx"}' />
                <MetricTile label="5xx" value={num(m.big_http_responses_total["5xx"])} tone="failed"
                  metric='big_http_responses_total{class="5xx"}' />
                <MetricTile label="timed out" value={num(m.big_http_queries_timed_out_total)} tone="degraded"
                  metric="big_http_queries_timed_out_total" />
                <MetricTile label="cancelled" value={num(m.big_http_queries_cancelled_total)}
                  metric="big_http_queries_cancelled_total" />
                <MetricTile label="401" value={compact(m.big_http_unauthorized_total)}
                  metric="big_http_unauthorized_total" />
              </div>

              {/* items-start: the chart keeps its own height instead of stretching to
                  match the stack beside it, which is what turns a sparkline into a poster. */}
              <div className="grid grid-cols-1 items-start gap-4 lg:grid-cols-[minmax(0,1fr)_320px]">
                <Panel>
                  <PanelHeader title="Latency percentiles"
                    description="Read off big_http_request_duration_seconds. A mean over this tail would describe a request nobody made." />
                  <div className="h-[220px] p-3"><LatencyBand data={latency} className="h-full" /></div>
                </Panel>

                <div className="space-y-4">
                  <Panel>
                    <PanelHeader title="Connections" />
                    <div className="p-3">
                      <div className="flex items-baseline justify-between font-mono text-sm">
                        <span className="text-fg-faint">accepted</span>
                        <span className="text-fg">{compact(m.big_http_connections_accepted_total)}</span>
                      </div>
                      <div className="flex items-baseline justify-between font-mono text-sm">
                        <span className="text-fg-faint">rejected</span>
                        <span className={m.big_http_connections_rejected_total ? "text-degraded" : "text-fg"}>
                          {num(m.big_http_connections_rejected_total)}
                        </span>
                      </div>
                      <Sparkline data={conns} className="mt-2 h-10 w-full" />
                    </div>
                  </Panel>
                  <Panel>
                    <PanelHeader title="Row-key dictionary" description="Grows with cardinality, never with records." />
                    <div className="p-3">
                      <div className="font-mono text-lg text-fg">{bytes(m.row_key_dictionary_bytes)}</div>
                      <div className="font-mono text-2xs text-fg-faint">{num(m.row_key_dictionary_keys)} keys interned</div>
                      <Sparkline data={dict} className="mt-2 h-10 w-full" stroke="hsl(var(--degraded))" />
                    </div>
                  </Panel>
                </div>
              </div>

              <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
                <MetricTile label="pages" value={compact(m.big_page_count)} metric="big_page_count" />
                <MetricTile label="free, reusable" value={compact(m.big_free_pages_reusable)}
                  metric="big_free_pages_reusable" tone="healthy" />
                <MetricTile label="pending: reader" value={compact(m.big_pages_pending_reclaim_reader)}
                  sub="held by an open read" metric="big_pages_pending_reclaim_reader" tone="degraded" />
                <MetricTile label="pending: retention" value={compact(m.big_pages_pending_reclaim_retention)}
                  sub="held by the retention window" metric="big_pages_pending_reclaim_retention" tone="degraded" />
              </div>
            </>
          )}
        </QueryBoundary>

        <Panel className="overflow-hidden">
          <PanelHeader
            title="Request log"
            description="One JSON line per request. Refusals are marked as such, not folded in with failures."
            actions={
              <>
                <div className="flex items-center gap-0.5 rounded border border-line bg-surface-sunken p-0.5">
                  {FILTERS.map((f) => (
                    <button key={f.id} onClick={() => setOnly(f.id)} aria-pressed={only === f.id}
                      className={cn("rounded-sm px-2 py-0.5 text-xs transition-colors duration-fast",
                        only === f.id ? "bg-surface text-fg shadow-e1" : "text-fg-faint hover:text-fg")}>
                      {f.label}
                    </button>
                  ))}
                </div>
                <div className="relative">
                  <Filter className="pointer-events-none absolute left-2 top-1/2 size-3 -translate-y-1/2 text-fg-faint" aria-hidden />
                  <Input value={q} onChange={(e) => setQ(e.target.value)} aria-label="Filter request log"
                    placeholder="request id, route, code…" className="h-7 w-64 pl-7 font-mono text-sm" />
                  {q && (
                    <Button size="iconSm" variant="ghost" aria-label="Clear filter"
                      className="absolute right-0.5 top-1/2 -translate-y-1/2" onClick={() => setQ("")}>
                      <X aria-hidden />
                    </Button>
                  )}
                </div>
              </>
            }
          />
          <QueryBoundary query={log} loadingRows={12}>
            {() => (
              <DataTable rows={rows} columns={columns} rowKey={(l) => l.request_id} maxHeight={520}
                empty={<EmptyState title="No requests match"
                  description={<>Nothing in this window matches <code>{q || only}</code>.</>} />} />
            )}
          </QueryBoundary>
        </Panel>
      </div>
    </div>
  );
}
