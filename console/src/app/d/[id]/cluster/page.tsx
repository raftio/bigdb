"use client";
import * as React from "react";
import { useQuery, useMutation } from "@tanstack/react-query";
import { Crown, PlayCircle, ShieldCheck, Wrench } from "lucide-react";
import { useClient, useDeployment } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/badges/status-badge";
import { HealthDot } from "@/components/badges/health-dot";
import { AgreementTable } from "@/components/data/diff-table";
import { QueryBoundary, EmptyState } from "@/components/data/states";
import { Guarded } from "@/components/data/guarded";
import { ConfirmDialog } from "@/components/data/confirm-dialog";
import { Tooltip } from "@/components/ui/tooltip";
import { absTime, num } from "@/lib/format";
import { cn } from "@/lib/cn";
import type { RangeInfo } from "@/lib/api/types";

/** Ranges from a config file, one copy serving each, one schema leader, CP. */
export default function ClusterPage() {
  const dep = useDeployment();
  const client = useClient();
  const [confirmRepair, setConfirmRepair] = React.useState(false);
  const [progress, setProgress] = React.useState<{ pct: number; note: string } | null>(null);

  const cluster = useQuery({
    queryKey: ["cluster", dep.data?.id], queryFn: () => client!.cluster(), enabled: !!client,
  });
  const verify = useMutation({ mutationFn: () => client!.verify() });
  const repair = useMutation({
    mutationFn: () => client!.repair((pct, note) => setProgress({ pct, note })),
    onSuccess: () => { setConfirmRepair(false); verify.reset(); },
  });

  const role = dep.data?.role ?? "read";
  const behind = cluster.data?.ranges.flatMap((r) => r.replicas.filter((x) => x.state !== "in_sync")) ?? [];

  if (dep.data && !dep.data.cluster) {
    return (
      <div className="mx-auto max-w-[1200px]">
        <PageHeader title="Cluster & ranges" />
        <div className="px-5 pb-8"><Panel><EmptyState
          title="This deployment is a single node"
          description="One bigd process over one file. There are no ranges to place and no copies to compare — /verify and /repair have nothing to do here."
        /></Panel></div>
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title="Cluster & ranges"
        description="Ranges come from a config file, and exactly one copy serves each. Failover is about a second. One schema leader owns every row key, and no read is ever answered from a copy known to be stale."
        actions={
          <>
            <Button variant="default" onClick={() => verify.mutate()} disabled={verify.isPending}>
              <ShieldCheck aria-hidden /> {verify.isPending ? "Verifying…" : "Run /verify"}
            </Button>
            <Guarded role={role} need="admin" what="Repair">
              {(ok) => (
                <Button variant="primary" disabled={!ok || repair.isPending} onClick={() => setConfirmRepair(true)}>
                  <Wrench aria-hidden /> Repair
                </Button>
              )}
            </Guarded>
          </>
        }
      >
        <div className="mt-2 flex flex-wrap gap-1.5">
          <Badge tone="accent">CP</Badge>
          <Badge>failover ≈ {cluster.data ? (cluster.data.failover_ms / 1000).toFixed(1) : "1.0"} s</Badge>
          <Badge>schema leader {cluster.data?.schema_leader ?? "—"}</Badge>
          <Badge>no stale reads</Badge>
        </div>
      </PageHeader>

      <div className="space-y-4 px-5 pb-8">
        <QueryBoundary query={cluster} loadingRows={6}
          unauthorized={{ have: role, need: "read", what: "Cluster topology" }}>
          {(c) => (
            <>
              <div className="grid grid-cols-1 gap-4 lg:grid-cols-[300px_minmax(0,1fr)]">
                <Panel className="h-fit">
                  <PanelHeader title="Nodes" description="Each answers /health and /ready for itself." />
                  <ul className="divide-y divide-line/60">
                    {c.nodes.map((n) => (
                      <li key={n.node} className="flex items-center gap-2 px-3 py-2">
                        <HealthDot status={n.status} showLabel={false} />
                        <span className="font-mono text-base text-fg">{n.node}</span>
                        {n.node === c.schema_leader && (
                          <Tooltip content="Owns every row key. Field and table creation is serialised through it, which is why the whole cluster agrees on the schema.">
                            <span className="inline-flex items-center gap-1 rounded-sm border border-degraded/40 bg-degraded-soft px-1 font-mono text-2xs text-degraded">
                              <Crown className="size-2.5" aria-hidden /> schema
                            </span>
                          </Tooltip>
                        )}
                        <span className="ml-auto font-mono text-2xs text-fg-faint">{n.region}</span>
                      </li>
                    ))}
                  </ul>
                </Panel>

                <Panel className="min-w-0">
                  <PanelHeader title="Ranges"
                    description="One copy serves a range. A replica behind the owner is never asked; the request is refused instead." />
                  <ul className="divide-y divide-line/60">
                    {c.ranges.map((r) => <RangeRow key={r.range} range={r} />)}
                  </ul>
                </Panel>
              </div>

              <Panel>
                <PanelHeader
                  title="Agreement"
                  description={verify.data
                    ? <>Checked {absTime(verify.data.checked_at)} &mdash; {verify.data.agreed
                        ? "every copy agrees." : "some copies disagree; /repair catches them up."}</>
                    : "GET /verify asks whether the copies of every range agree. It reads; it changes nothing."}
                  actions={verify.data && (
                    <Badge tone={verify.data.agreed ? "healthy" : "degraded"}>
                      {verify.data.agreed ? "all agree" : `${verify.data.rows.flatMap((r) => r.replicas.filter((x) => !x.agrees)).length} disagree`}
                    </Badge>
                  )}
                />
                {verify.isPending && <div className="px-3 py-8 text-center text-base text-fg-muted">Comparing every range on every copy…</div>}
                {!verify.isPending && !verify.data && (
                  <EmptyState title="Not checked in this session" icon={PlayCircle}
                    description="Running /verify walks every range on every copy and compares checksums. It is a read, so it is safe to run whenever."
                    action={<Button variant="primary" onClick={() => verify.mutate()}><ShieldCheck aria-hidden /> Run /verify</Button>} />
                )}
                {verify.data && <AgreementTable result={verify.data} />}
              </Panel>
            </>
          )}
        </QueryBoundary>
      </div>

      <ConfirmDialog
        open={confirmRepair} onOpenChange={setConfirmRepair}
        title="Repair every range that is behind"
        tone="caution"
        confirmLabel="Run /repair"
        busy={repair.isPending}
        onConfirm={() => repair.mutate()}
        blastRadius={<>
          <b>Cluster-wide.</b> POST /repair streams fragments from each range&rsquo;s owner to every copy that is
          behind &mdash; currently <b>{behind.length}</b>{" "}
          {behind.length === 1 ? "replica" : "replicas"}
          {behind.length > 0 && <> ({num(behind.reduce((a, b) => a + Math.max(0, b.lag_records), 0))} records)</>}.
          Owners keep serving throughout; the extra load lands on them.
        </>}
        detail={progress
          ? <div className="space-y-1.5">
              <div className="h-1.5 w-full overflow-hidden rounded-sm bg-surface-sunken">
                <div className="h-full bg-accent transition-[width] duration-150" style={{ width: `${progress.pct}%` }} />
              </div>
              <div className="font-mono text-2xs text-fg-muted">{progress.pct}% &mdash; {progress.note}</div>
            </div>
          : "An unreachable copy is skipped and retried; the operation is safe to run again."}
      />
    </div>
  );
}

function RangeRow({ range }: { range: RangeInfo }) {
  const TONE = { in_sync: "healthy", behind: "degraded", unreachable: "failed" } as const;
  return (
    <li className="grid grid-cols-[80px_150px_1fr] items-center gap-3 px-3 py-2">
      <span className="font-mono text-base text-fg">{range.range}</span>
      <span className="flex items-center gap-1.5">
        <span className="text-2xs uppercase tracking-wider text-fg-faint">owner</span>
        <span className="font-mono text-base text-accent">{range.owner}</span>
      </span>
      <span className="flex flex-wrap items-center gap-1.5">
        {range.replicas.map((r) => (
          <Tooltip key={r.node} content={r.state === "in_sync"
            ? "Caught up with the owner."
            : r.state === "behind"
              ? `${num(r.lag_records)} records behind, ${num(r.lag_ms)} ms of replication lag. It will not be asked to serve a read.`
              : "No response. Its share of the range is served by the owner alone."}>
            <span className={cn(
              "inline-flex items-center gap-1 rounded-sm border px-1.5 py-px font-mono text-2xs",
              r.state === "in_sync" ? "border-line-strong text-fg-muted"
                : r.state === "behind" ? "border-degraded/45 bg-degraded-soft text-degraded"
                : "border-failed/45 bg-failed-soft text-failed",
            )}>
              {r.node}
              {r.state !== "in_sync" && <span>{r.state === "behind" ? `−${num(r.lag_records)}` : "unreachable"}</span>}
            </span>
          </Tooltip>
        ))}
      </span>
    </li>
  );
}
