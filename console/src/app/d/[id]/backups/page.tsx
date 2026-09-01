"use client";
import * as React from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { HardDriveDownload, Terminal } from "lucide-react";
import { useClient, useDeployment } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input, Field } from "@/components/ui/input";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";
import { Badge } from "@/components/badges/status-badge";
import { DataTable, type Column } from "@/components/data/data-table";
import { QueryBoundary, EmptyState } from "@/components/data/states";
import { Guarded } from "@/components/data/guarded";
import { ConfirmDialog } from "@/components/data/confirm-dialog";
import { CLUSTER } from "@/lib/api/mock/fixtures";
import { absTime, bytes, duration, pct } from "@/lib/format";
import type { Backup } from "@/lib/api/types";

/**
 * A backup is a copy of the live pages into a fresh file -- which is also what
 * a compaction is, and what a format migration is. One operation, three names.
 *
 * The screen is explicit that a cluster backup is one node at a time, so the
 * set of files is not one snapshot. That is the single most important thing an
 * operator can misunderstand here, so it is stated on the page, not in a doc.
 */
export default function BackupsPage() {
  const dep = useDeployment();
  const client = useClient();
  const qc = useQueryClient();
  const [taking, setTaking] = React.useState(false);
  const [name, setName] = React.useState(`manual-${new Date().toISOString().slice(0, 10)}`);
  const [node, setNode] = React.useState(CLUSTER.nodes[0].node);

  const backups = useQuery({
    queryKey: ["backups", dep.data?.id], queryFn: () => client!.backups(), enabled: !!client,
  });
  const take = useMutation({
    mutationFn: () => client!.backup(name, node),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ["backups"] }); setTaking(false); },
  });

  const role = dep.data?.role ?? "read";

  const columns: Column<Backup>[] = [
    { key: "name", header: "name", width: 200, mono: true,
      cell: (b) => <span className="text-fg">{b.name}</span>, sort: (a, b) => a.name.localeCompare(b.name) },
    { key: "node", header: "node", width: 84, mono: true, cell: (b) => <span className="text-fg-muted">{b.node}</span> },
    { key: "started", header: "started", width: 190, mono: true,
      cell: (b) => <span className="text-fg-muted">{absTime(b.started_at)}</span>,
      sort: (a, b) => a.started_at.localeCompare(b.started_at) },
    { key: "duration", header: "duration", width: 92, align: "right", mono: true,
      cell: (b) => b.status === "complete" ? duration(b.duration_ms) : <span className="text-fg-faint">&mdash;</span>,
      sort: (a, b) => a.duration_ms - b.duration_ms },
    { key: "size", header: "size", width: 96, align: "right", mono: true,
      cell: (b) => b.status === "complete" ? <span className="text-fg">{bytes(b.bytes)}</span> : <span className="text-fg-faint">&mdash;</span>,
      sort: (a, b) => a.bytes - b.bytes },
    { key: "reclaimed", header: "reclaimed", width: 100, align: "right", mono: true,
      cell: (b) => b.status === "complete"
        ? <span className="text-healthy" title="Pages the copy left behind — the compaction a backup performs for free">
            {pct(1 - b.bytes / b.source_bytes, 1)}
          </span>
        : <span className="text-fg-faint">&mdash;</span> },
    { key: "format", header: "format", width: 74, mono: true, cell: (b) => <span className="text-fg-faint">v{b.format_version}</span> },
    { key: "status", header: "status",
      cell: (b) => b.status === "complete"
        ? <Badge tone="healthy">complete</Badge>
        : b.status === "running"
          ? <Badge tone="accent">running</Badge>
          : <span className="flex items-center gap-2">
              <Badge tone="failed">failed</Badge>
              <span className="truncate font-mono text-2xs text-failed" title={b.error}>{b.error}</span>
            </span> },
  ];

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title="Backups"
        description="Backup, compaction and format migration are the same operation: copy the live pages into a fresh file. Which is why every completed backup also reports what it reclaimed."
        actions={
          <Guarded role={role} need="admin" what="Taking a backup">
            {(ok) => (
              <Button variant="primary" disabled={!ok} onClick={() => setTaking(true)}>
                <HardDriveDownload aria-hidden /> Back up now
              </Button>
            )}
          </Guarded>
        }
      />

      <div className="space-y-4 px-5 pb-8">
        <Panel className="border-degraded/35">
          <div className="flex gap-3 p-3.5">
            <span className="mt-0.5 shrink-0 text-degraded" aria-hidden>&#9888;</span>
            <div className="space-y-1.5 text-base leading-relaxed text-fg-muted">
              <p className="font-medium text-fg">A cluster backup is one node at a time. The copies are not one snapshot.</p>
              <p>
                <code>POST /admin/backup</code> copies <b>this node&rsquo;s file</b>. Running it across four nodes gives
                four files taken at four different instants &mdash; each internally consistent, none consistent with the
                others. To restore a cluster you restore per node and then let <code>/repair</code> reconcile the ranges.
              </p>
            </div>
          </div>
        </Panel>

        <Panel className="overflow-hidden">
          <PanelHeader title="History" description="Per node, newest first." />
          <QueryBoundary query={backups} loadingRows={6}
            unauthorized={{ have: role, need: "admin", what: "Backups" }}>
            {(rows) => (
              <DataTable rows={rows} columns={columns} rowKey={(b) => b.id}
                empty={<EmptyState title="No backups yet"
                  description="Nothing has been copied out of this deployment. A backup is also the only way to reclaim pages back to the filesystem." />} />
            )}
          </QueryBoundary>
        </Panel>

        <Panel>
          <PanelHeader title="Restoring" description="There is no restore API — a restore is a file swap, done with the process stopped." />
          <div className="space-y-2 p-3.5">
            <ol className="space-y-1.5 text-base leading-relaxed text-fg-muted">
              <li><span className="font-mono text-fg-faint">1.</span> Stop <code>bigd</code> on the node you are restoring.</li>
              <li><span className="font-mono text-fg-faint">2.</span> Put the backup file where the data file was.</li>
              <li><span className="font-mono text-fg-faint">3.</span> Start <code>bigd</code>. There is no WAL to replay: the meta page in the file is the state.</li>
              <li><span className="font-mono text-fg-faint">4.</span> On a cluster, run <code>/verify</code>, then <code>/repair</code> to bring the restored node&rsquo;s ranges level with their owners.</li>
            </ol>
            <pre className="mt-2 overflow-x-auto rounded border border-line bg-surface-sunken px-2.5 py-2 text-sm leading-relaxed text-fg-muted"><code>{`systemctl stop bigd
mv /var/lib/big/data.big /var/lib/big/data.big.old
cp  /backups/nightly-2026-08-31.big /var/lib/big/data.big
systemctl start bigd
curl -sH "Authorization: Bearer $ADMIN" $HOST/verify`}</code></pre>
            <p className="flex items-center gap-1.5 text-xs text-fg-faint">
              <Terminal className="size-3" aria-hidden /> The console does not run this for you: it stops a process on a machine it does not own.
            </p>
          </div>
        </Panel>
      </div>

      <ConfirmDialog
        open={taking} onOpenChange={setTaking}
        title="Back up now"
        tone="caution"
        confirmLabel="Start backup"
        busy={take.isPending}
        onConfirm={() => take.mutate()}
        blastRadius={<>
          Copies <b>{node}</b>&rsquo;s file only &mdash; not the cluster. The node keeps serving; the copy competes for
          disk throughput while it runs, so p95 on that node will rise.
        </>}
        detail={
          <div className="space-y-3">
            <Field label="Name" hint="Backups are named, not versioned. Two backups may share a name across nodes; that is how a nightly set is grouped.">
              <Input value={name} onChange={(e) => setName(e.target.value)} className="font-mono" />
            </Field>
            <Field label="Node" hint="One node per call. POST /admin/backup copies the file of the process it reaches.">
              <Select value={node} onValueChange={setNode}>
                <SelectTrigger className="w-full font-mono"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {CLUSTER.nodes.map((n) => <SelectItem key={n.node} value={n.node} hint={n.region}>{n.node}</SelectItem>)}
                </SelectContent>
              </Select>
            </Field>
          </div>
        }
      />
    </div>
  );
}
