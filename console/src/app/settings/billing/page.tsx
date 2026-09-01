"use client";
import * as React from "react";
import Link from "next/link";
import { useQuery } from "@tanstack/react-query";
import { Download } from "lucide-react";
import { controlPlane } from "@/lib/api/client";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/badges/status-badge";
import { MetricTile } from "@/components/data/metric-tile";
import { DataTable, type Column } from "@/components/data/data-table";
import { QueryBoundary } from "@/components/data/states";
import { Tooltip } from "@/components/ui/tooltip";
import { bytes, compact, num } from "@/lib/format";
import type { Invoice, UsageRow } from "@/lib/api/types";

const usd = (n: number) => `$${n.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;

/** Usage is metered on the three things the engine actually spends: queries, facts, pages. */
export default function BillingPage() {
  const usage = useQuery({ queryKey: ["usage"], queryFn: controlPlane.usage });
  const invoices = useQuery({ queryKey: ["invoices"], queryFn: controlPlane.invoices });

  const totals = React.useMemo(() => {
    const rows = usage.data ?? [];
    return {
      queries: rows.reduce((a, r) => a + r.queries, 0),
      facts: rows.reduce((a, r) => a + r.facts_ingested, 0),
      pages: rows.reduce((a, r) => a + r.storage_pages, 0),
      cost: rows.reduce((a, r) => a + r.cost_usd, 0),
    };
  }, [usage.data]);

  const usageCols: Column<UsageRow>[] = [
    { key: "deployment", header: "deployment", width: 200, mono: true,
      cell: (r) => <Link href={`/d/${r.deployment_id}`} className="text-fg hover:text-accent">{r.deployment}</Link>,
      sort: (a, b) => a.deployment.localeCompare(b.deployment) },
    { key: "queries", header: "queries", width: 120, align: "right", mono: true,
      cell: (r) => <Tooltip content={num(r.queries)}><span className="text-fg">{compact(r.queries)}</span></Tooltip>,
      sort: (a, b) => a.queries - b.queries },
    { key: "facts", header: "facts ingested", width: 140, align: "right", mono: true,
      cell: (r) => <Tooltip content={num(r.facts_ingested)}><span className="text-fg">{compact(r.facts_ingested)}</span></Tooltip>,
      sort: (a, b) => a.facts_ingested - b.facts_ingested },
    { key: "pages", header: "storage pages", width: 140, align: "right", mono: true,
      cell: (r) => <Tooltip content={`${num(r.storage_pages)} pages · ${bytes(r.storage_pages * 4096)}`}>
        <span className="text-fg-muted">{compact(r.storage_pages)}</span></Tooltip>,
      sort: (a, b) => a.storage_pages - b.storage_pages },
    { key: "cost", header: "cost", align: "right", mono: true,
      cell: (r) => <span className="text-fg">{usd(r.cost_usd)}</span>, sort: (a, b) => a.cost_usd - b.cost_usd },
  ];

  const invoiceCols: Column<Invoice>[] = [
    { key: "period", header: "period", width: 170, cell: (i) => <span className="text-fg">{i.period}</span> },
    { key: "id", header: "invoice", width: 130, mono: true, cell: (i) => <span className="text-fg-muted">{i.id}</span> },
    { key: "issued", header: "issued", width: 130, mono: true, cell: (i) => <span className="text-fg-muted">{i.issued_at}</span> },
    { key: "total", header: "total", width: 120, align: "right", mono: true,
      cell: (i) => <span className="text-fg">{usd(i.total_usd)}</span> },
    { key: "status", header: "status", width: 110,
      cell: (i) => <Badge tone={i.status === "paid" ? "healthy" : i.status === "open" ? "accent" : "failed"}>{i.status}</Badge> },
    { key: "pdf", header: "", align: "right",
      cell: () => <Button size="xs" variant="ghost"><Download aria-hidden /> PDF</Button> },
  ];

  return (
    <div className="mx-auto max-w-[1400px]">
      <PageHeader
        title="Billing & usage"
        description="Metered on what the engine spends: queries answered, facts ingested, and pages held. Pages are the honest storage number — a backup that compacts lowers the bill."
        actions={<Button variant="default">Change plan</Button>}
      />

      <div className="space-y-4 px-5 pb-8">
        <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
          <MetricTile label="queries this period" value={compact(totals.queries)} tone="accent" />
          <MetricTile label="facts ingested" value={compact(totals.facts)} />
          <MetricTile label="storage pages" value={compact(totals.pages)} sub={bytes(totals.pages * 4096, 0)} />
          <MetricTile label="running total" value={usd(totals.cost)} />
        </div>

        <Panel className="overflow-hidden">
          <PanelHeader title="Usage by deployment" description="August 2026, to date." />
          <QueryBoundary query={usage} loadingRows={5}>
            {(rows) => <DataTable rows={rows} columns={usageCols} rowKey={(r) => r.deployment_id} />}
          </QueryBoundary>
        </Panel>

        <Panel className="overflow-hidden">
          <PanelHeader title="Invoices" />
          <QueryBoundary query={invoices} loadingRows={4}>
            {(rows) => <DataTable rows={rows} columns={invoiceCols} rowKey={(i) => i.id} />}
          </QueryBoundary>
        </Panel>
      </div>
    </div>
  );
}
