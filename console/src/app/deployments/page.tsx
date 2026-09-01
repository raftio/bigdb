"use client";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { Plus } from "lucide-react";
import { useDeployments } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { DataTable, type Column } from "@/components/data/data-table";
import { QueryBoundary, EmptyState } from "@/components/data/states";
import { HealthDot } from "@/components/badges/health-dot";
import { EngineMix } from "@/components/badges/engine-badge";
import { RoleBadge } from "@/components/badges/role-badge";
import { Button } from "@/components/ui/button";
import { Panel } from "@/components/ui/card";
import { bytes, compact, ms, num } from "@/lib/format";
import type { Deployment } from "@/lib/api/types";
import { Tooltip } from "@/components/ui/tooltip";

/**
 * Every `bigd` a team runs. One row is one process over one file — the unit the
 * whole product is built on, so the row carries the four numbers that decide
 * whether you need to look closer: size, throughput, tail latency, health.
 */
export default function DeploymentsPage() {
  const q = useDeployments();
  const router = useRouter();

  const columns: Column<Deployment>[] = [
    {
      key: "name", header: "deployment", width: 250,
      cell: (d) => (
        <span className="flex items-center gap-2">
          <HealthDot status={d.status} showLabel={false} />
          <span className="truncate font-mono text-base text-fg">{d.name}</span>
          {d.cluster && <Tooltip content="Ranges come from a config file; one copy serves a range."><span className="shrink-0 rounded-sm border border-line-strong px-1 font-mono text-2xs text-fg-faint">cluster</span></Tooltip>}
        </span>
      ),
      sort: (a, b) => a.name.localeCompare(b.name),
    },
    { key: "region", header: "region / host", width: 230, mono: true,
      cell: (d) => <span className="truncate text-fg-muted" title={d.host}>{d.region} · {d.host.split(".")[0]}</span>,
      sort: (a, b) => a.region.localeCompare(b.region) },
    { key: "engines", header: "engine mix", width: 120, cell: (d) => <EngineMix engines={d.engines} /> },
    { key: "tables", header: "tables", width: 66, align: "right", mono: true, cell: (d) => num(d.table_count),
      sort: (a, b) => a.table_count - b.table_count },
    { key: "file", header: "file", width: 92, align: "right", mono: true,
      cell: (d) => <span className="text-fg">{bytes(d.file_bytes, 0)}</span>, sort: (a, b) => a.file_bytes - b.file_bytes },
    { key: "pages", header: "pages", width: 92, align: "right", mono: true,
      cell: (d) => <Tooltip content={<code className="text-2xs">big_page_count = {num(d.page_count)}</code>}><span className="text-fg-muted">{compact(d.page_count)}</span></Tooltip>,
      sort: (a, b) => a.page_count - b.page_count },
    { key: "qpm", header: "queries/min", width: 100, align: "right", mono: true,
      cell: (d) => d.qpm ? num(d.qpm) : <span className="text-fg-faint">—</span>, sort: (a, b) => a.qpm - b.qpm },
    { key: "p95", header: "p95", width: 84, align: "right", mono: true,
      cell: (d) => d.p95_ms ? <span className={d.p95_ms > 10 ? "text-degraded" : "text-fg"}>{ms(d.p95_ms)}</span> : <span className="text-fg-faint">—</span>,
      sort: (a, b) => a.p95_ms - b.p95_ms },
    { key: "role", header: "token", width: 72, cell: (d) => <RoleBadge role={d.role} /> },
    { key: "version", header: "version", width: 74, mono: true, cell: (d) => <span className="text-fg-faint">{d.version}</span> },
  ];

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title="Deployments"
        description="A deployment is one bigd process over one file, with its own token set. Table names are a flat global namespace inside one deployment — never across them."
        actions={<Button variant="primary" asChild><Link href="/deployments/new"><Plus aria-hidden /> New deployment</Link></Button>}
      />
      <div className="px-5 pb-8">
        <Panel className="overflow-hidden">
          <QueryBoundary
            query={q}
            empty={(d) => d.length === 0}
          >
            {(rows) => (
              <DataTable
                rows={rows}
                columns={columns}
                rowKey={(d) => d.id}
                onRowClick={(d) => router.push(`/d/${d.id}`)}
                empty={<EmptyState
                  title="No deployments yet"
                  description="A deployment is one bigd process over one file. Create one and it comes up empty, with an admin token you see exactly once."
                  action={<Button variant="primary" asChild><Link href="/deployments/new"><Plus aria-hidden /> New deployment</Link></Button>}
                />}
              />
            )}
          </QueryBoundary>
        </Panel>
      </div>
    </div>
  );
}
