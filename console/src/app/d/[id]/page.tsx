"use client";
import * as React from "react";
import Link from "next/link";
import { Terminal, Boxes, ArrowUpRight } from "lucide-react";
import { useDeployment, useMetrics } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { MetricTile } from "@/components/data/metric-tile";
import { LatencyBand } from "@/components/data/sparkline";
import { ArchitectureDiagram, DictionaryGauge } from "@/components/data/architecture-diagram";
import { QueryBoundary, QuietError } from "@/components/data/states";
import { RefusalLine } from "@/components/data/refusal-panel";
import { HealthDot } from "@/components/badges/health-dot";
import { Badge } from "@/components/badges/status-badge";
import { Button } from "@/components/ui/button";
import { bytes, compact, ms, num } from "@/lib/format";
import { latencySeries, series, requestLog } from "@/lib/api/mock/fixtures";
import { isRefusal } from "@/lib/refusals";

export default function OverviewPage() {
  const dep = useDeployment();
  const metrics = useMetrics();
  const base = dep.data ? `/d/${dep.data.id}` : "";

  const qps = React.useMemo(() => series(11, 60, 240, 90), []);
  const latency = React.useMemo(() => latencySeries(23, 60), []);
  const pages = React.useMemo(() => series(31, 60, 36_000_000, 40_000, 3_000), []);
  const pending = React.useMemo(() => series(41, 60, 170_000, 60_000), []);
  const errors = React.useMemo(() => byCode(), []);

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title={<>
          <span className="font-mono">{dep.data?.name ?? "…"}</span>
          {dep.data && <HealthDot status={dep.data.status} />}
        </>}
        description={dep.data && <>
          <code>{dep.data.host}</code> · {dep.data.region} · bigd {dep.data.version} · {bytes(dep.data.file_bytes)} in one file
        </>}
        actions={<>
          <Button variant="default" asChild><Link href={`${base}/schema`}><Boxes aria-hidden /> Schema</Link></Button>
          <Button variant="primary" asChild><Link href={`${base}/query`}><Terminal aria-hidden /> Query</Link></Button>
        </>}
      />

      <div className="grid grid-cols-1 gap-4 px-5 pb-8 xl:grid-cols-[minmax(0,1fr)_380px]">
        <div className="min-w-0 space-y-4">
          <QueryBoundary query={metrics} loadingRows={4} unauthorized={{ have: dep.data?.role ?? "read", need: "read", what: "Metrics" }}>
            {(m) => (
              <>
                <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
                  <MetricTile label="queries / min" value={num(qps[qps.length - 1].v)} series={qps}
                    metric="rate(big_http_requests_total[1m]) × 60" tone="accent" />
                  <MetricTile label="p95" value={ms(m.latency.p95_ms)} sub={`p99 ${ms(m.latency.p99_ms)}`}
                    metric="histogram_quantile(0.95, big_http_request_duration_seconds)"
                    tone={m.latency.p95_ms > 10 ? "degraded" : "default"} />
                  <MetricTile label="pages" value={compact(m.big_page_count)} sub={bytes(m.big_page_count * 4096, 0)}
                    series={pages} metric="big_page_count" />
                  <MetricTile label="pending reclaim" value={compact(m.big_pages_pending_reclaim_reader + m.big_pages_pending_reclaim_retention)}
                    sub={`${compact(m.big_free_pages_reusable)} reusable`} series={pending}
                    metric="big_pages_pending_reclaim_reader + big_pages_pending_reclaim_retention" tone="degraded" />
                </div>

                <Panel>
                  <PanelHeader title="Request latency"
                    description="p50 line inside the p95–p99 band, read off the histogram. There is no mean here on purpose."
                    actions={<Button size="xs" variant="ghost" asChild><Link href={`${base}/observability`}>Open <ArrowUpRight aria-hidden /></Link></Button>} />
                  <div className="h-[132px] p-3"><LatencyBand data={latency} className="h-full" /></div>
                </Panel>

                <Panel>
                  <PanelHeader title="Request path" description="Each layer with its own number, top to bottom." />
                  <div className="p-3"><ArchitectureDiagram metrics={m} cluster={!!dep.data?.cluster} /></div>
                </Panel>
              </>
            )}
          </QueryBoundary>
        </div>

        <div className="min-w-0 space-y-4">
          <QueryBoundary query={metrics} loadingRows={3}
            errorFallback={<Panel><QuietError note="Dictionary size is read from /metrics, which this deployment did not answer." /></Panel>}>
            {(m) => <DictionaryGauge metrics={m} ceilingBytes={8 * 1024 ** 3} />}
          </QueryBoundary>

          <Panel>
            <PanelHeader title="Commit model" />
            <div className="space-y-2 p-3.5 text-base leading-relaxed text-fg-muted">
              <p>
                There is <b className="text-fg">no WAL</b>. A commit writes pages, fsyncs, flips the meta page and
                fsyncs again. There is no in-between state to recover from — the file is either the old meta or the new one.
              </p>
              <p>
                Backup, compaction and format migration are the <b className="text-fg">same operation</b>: copy the live
                pages into a fresh file. Which is why{" "}
                <Link href={`${base}/backups`} className="text-accent hover:underline">a backup also compacts</Link>.
              </p>
            </div>
          </Panel>

          <Panel>
            <PanelHeader title="Recent responses by code" description="Refusals are separated from failures — they are not the same event." />
            {metrics.error ? <QuietError note="No window to summarise: this deployment is not answering." /> : (<>
            <ul className="divide-y divide-line">
              {errors.map((e) => (
                <li key={e.code} className="flex items-center gap-2 px-3 py-2">
                  <Badge tone={e.refused ? "refused" : e.status >= 500 ? "failed" : "degraded"}>{e.status}</Badge>
                  <code className="min-w-0 flex-1 truncate text-sm text-fg">{e.code}</code>
                  <span className="shrink-0 font-mono text-sm text-fg-muted">{num(e.n)}</span>
                </li>
              ))}
              {!errors.length && <li className="px-3 py-6 text-center text-base text-fg-faint">Nothing but 2xx in this window.</li>}
            </ul>
            <div className="border-t border-line px-3 py-2">
              <RefusalLine code="sql_no_joins" message="no joins" />
            </div>
            </>)}
          </Panel>
        </div>
      </div>
    </div>
  );
}

function byCode() {
  const log = requestLog(400).filter((l) => l.status >= 400 && l.code);
  const m = new Map<string, { code: string; status: number; n: number; refused: boolean }>();
  for (const l of log) {
    const k = l.code!;
    const e = m.get(k) ?? { code: k, status: l.status, n: 0, refused: isRefusal(k) };
    e.n++;
    m.set(k, e);
  }
  return [...m.values()].sort((a, b) => b.n - a.n).slice(0, 8);
}
