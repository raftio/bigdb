"use client";
import * as React from "react";
import Link from "next/link";
import { useParams } from "next/navigation";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, Plus, Trash2 } from "lucide-react";
import { useClient, useDeployment, useSchema } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { DataTable, type Column } from "@/components/data/data-table";
import { BarRow } from "@/components/data/sparkline";
import { QueryBoundary, EmptyState } from "@/components/data/states";
import { EngineBadge } from "@/components/badges/engine-badge";
import { FieldKindBadge } from "@/components/schema/field-kind-badge";
import { Guarded } from "@/components/data/guarded";
import { ConfirmDialog } from "@/components/data/confirm-dialog";
import { CreateFieldDialog } from "@/components/schema/create-dialogs";
import { Tooltip } from "@/components/ui/tooltip";
import { bytes, compact, num } from "@/lib/format";
import { CARDINALITY, RECORD_COUNTS } from "@/lib/api/mock/fixtures";
import type { Fragment } from "@/lib/api/types";

/**
 * Inside a table, storage is addressable by the fragment key: (table, field,
 * view, shard). That tuple is the primary key of this screen -- it is what the
 * engine names a file region by, so it is what the console names a row by.
 */
export default function TableDetailPage() {
  const { table } = useParams<{ table: string }>();
  const dep = useDeployment();
  const client = useClient();
  const schema = useSchema();
  const qc = useQueryClient();
  const [filter, setFilter] = React.useState("");
  const [creatingField, setCreatingField] = React.useState(false);
  const [droppingField, setDroppingField] = React.useState<string | null>(null);

  const frags = useQuery({
    queryKey: ["fragments", dep.data?.id, table],
    queryFn: () => client!.fragments(table),
    enabled: !!client,
  });

  const dropField = useMutation({
    mutationFn: (f: string) => client!.dropField(table, f),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ["schema"] }); setDroppingField(null); },
  });

  const info = schema.data?.tables.find((t) => t.name === table);
  const role = dep.data?.role ?? "read";

  const columns: Column<Fragment>[] = [
    { key: "field", header: "field", width: 150, mono: true, cell: (f) => <span className="text-fg">{f.field}</span>,
      sort: (a, b) => a.field.localeCompare(b.field) },
    { key: "view", header: "view", width: 120, mono: true,
      cell: (f) => <Tooltip content={f.view.startsWith("standard_")
        ? `One view per granularity: ${f.view.slice(-1)}. A time window is answered from the coarsest view that covers it.`
        : f.view === "bsi" ? "Bit-sliced planes for an integer or decimal field." : "The default view."}>
        <span className="text-fg-muted">{f.view}</span></Tooltip>,
      sort: (a, b) => a.view.localeCompare(b.view) },
    { key: "shard", header: "shard", width: 66, align: "right", mono: true, cell: (f) => f.shard,
      sort: (a, b) => a.shard - b.shard },
    { key: "bytes", header: "size", width: 92, align: "right", mono: true,
      cell: (f) => <span className="text-fg">{bytes(f.bytes, 1)}</span>, sort: (a, b) => a.bytes - b.bytes },
    { key: "cardinality", header: "keys", width: 84, align: "right", mono: true,
      cell: (f) => <span className="text-fg-muted">{compact(f.cardinality)}</span>, sort: (a, b) => a.cardinality - b.cardinality },
    {
      key: "containers", header: "container mix", width: 200,
      cell: (f) => (
        <Tooltip content={<div className="space-y-0.5 font-mono text-2xs">
          <div>array &nbsp;{num(f.containers.array)} &mdash; sparse, a sorted list of set bits</div>
          <div>bitmap {num(f.containers.bitmap)} &mdash; dense, one bit per position</div>
          <div>run &nbsp;&nbsp;&nbsp;{num(f.containers.run)} &mdash; long consecutive spans</div>
        </div>}>
          <span className="block">
            <BarRow segments={[
              { label: "array", value: f.containers.array, color: "bg-accent/70" },
              { label: "bitmap", value: f.containers.bitmap, color: "bg-viz-p95" },
              { label: "run", value: f.containers.run, color: "bg-viz-p99" },
            ]} />
          </span>
        </Tooltip>
      ),
    },
    {
      key: "key", header: "fragment key", mono: true,
      cell: (f) => <span className="truncate text-fg-faint">({f.table}, {f.field}, {f.view}, {f.shard})</span>,
    },
  ];

  const filtered = (frags.data ?? []).filter((f) =>
    !filter || `${f.field} ${f.view} ${f.shard}`.toLowerCase().includes(filter.toLowerCase()));

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title={<>
          <Button size="iconSm" variant="ghost" asChild aria-label="Back to schema">
            <Link href={`/d/${dep.data?.id}/schema`}><ArrowLeft aria-hidden /></Link>
          </Button>
          <span className="font-mono">{table}</span>
          {info && <EngineBadge engine={info.engine} />}
        </>}
        description={info && <span className="flex flex-wrap gap-x-4 font-mono">
          <span>{num(RECORD_COUNTS[table] ?? 0)} records</span>
          <span>{info.fields.length} fields</span>
          <span>{num(frags.data?.length ?? 0)} fragments</span>
          <span>{bytes((frags.data ?? []).reduce((a, f) => a + f.bytes, 0))}</span>
        </span>}
        actions={
          <Guarded role={role} need="admin" what="Creating a field">
            {(ok) => <Button variant="primary" disabled={!ok} onClick={() => setCreatingField(true)}><Plus aria-hidden /> New field</Button>}
          </Guarded>
        }
      />

      <div className="grid grid-cols-1 gap-4 px-5 pb-8 xl:grid-cols-[320px_minmax(0,1fr)]">
        <Panel className="h-fit">
          <PanelHeader title="Fields" description="Kind, bit depth and scale — the three things a client cannot derive." />
          <ul className="divide-y divide-line/60">
            {info?.fields.map((f) => (
              <li key={f.name} className="flex items-center gap-2 px-3 py-2">
                <div className="min-w-0 flex-1">
                  <div className="truncate font-mono text-base text-fg">{f.name}</div>
                  <div className="mt-0.5"><FieldKindBadge field={f} /></div>
                </div>
                <span className="shrink-0 font-mono text-2xs text-fg-faint">
                  {CARDINALITY[table]?.[f.name] ? compact(CARDINALITY[table][f.name]) : "—"}
                </span>
                <Guarded role={role} need="admin" what="Dropping a field">
                  {(ok) => (
                    <Button size="iconSm" variant="ghost" disabled={!ok} onClick={() => setDroppingField(f.name)}
                      aria-label={`Drop field ${f.name}`}>
                      <Trash2 className={ok ? "text-failed" : undefined} aria-hidden />
                    </Button>
                  )}
                </Guarded>
              </li>
            ))}
          </ul>
        </Panel>

        <Panel className="min-w-0 overflow-hidden">
          <PanelHeader
            title="Fragments"
            description="Addressed by (table, field, view, shard) — the key the storage layer itself uses."
            actions={<Input value={filter} onChange={(e) => setFilter(e.target.value)}
              placeholder="filter by field, view or shard" className="h-7 w-56 font-mono text-sm" />}
          />
          <QueryBoundary query={frags} loadingRows={10}>
            {() => (
              <DataTable rows={filtered} columns={columns}
                rowKey={(f) => `${f.field}/${f.view}/${f.shard}`} maxHeight={560}
                empty={<EmptyState title="No fragments match" description={<>Nothing in <code>{table}</code> matches <code>{filter}</code>.</>} />}
              />
            )}
          </QueryBoundary>
        </Panel>
      </div>

      {client && <CreateFieldDialog open={creatingField} onOpenChange={setCreatingField} client={client} table={table} />}
      {droppingField && (
        <ConfirmDialog
          open onOpenChange={(v) => !v && setDroppingField(null)}
          title={`Drop field ${droppingField}`}
          confirmWord={droppingField}
          confirmLabel="Drop field"
          busy={dropField.isPending}
          onConfirm={() => dropField.mutate(droppingField)}
          blastRadius={<>
            Every fragment of <code>{table}/{droppingField}</code> across{" "}
            <b>{num((frags.data ?? []).filter((f) => f.field === droppingField).length)} (view, shard) pairs</b> is
            unlinked, on the owner and on every replica. Queries naming it start returning{" "}
            <code>unknown_field</code> at parse time.
          </>}
          detail="The row-key dictionary entries for this field go with it, which is the memory you get back."
        />
      )}
    </div>
  );
}
