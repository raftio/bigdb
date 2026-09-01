"use client";
import * as React from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Plus, Trash2 } from "lucide-react";
import { useClient, useDeployment, useSchema } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { EngineBadge } from "@/components/badges/engine-badge";
import { FieldKindBadge } from "@/components/schema/field-kind-badge";
import { QueryBoundary, EmptyState } from "@/components/data/states";
import { Guarded } from "@/components/data/guarded";
import { ConfirmDialog } from "@/components/data/confirm-dialog";
import { CreateTableDialog } from "@/components/schema/create-dialogs";
import { CARDINALITY, RECORD_COUNTS, fragmentsFor } from "@/lib/api/mock/fixtures";
import { bytes, compact, num } from "@/lib/format";
import type { TableInfo } from "@/lib/api/types";

/** GET /schema, laid out so a table's engine and a field's kind are never far from its name. */
export default function SchemaPage() {
  const dep = useDeployment();
  const client = useClient();
  const schema = useSchema();
  const router = useRouter();
  const qc = useQueryClient();
  const [creating, setCreating] = React.useState(false);
  const [dropping, setDropping] = React.useState<TableInfo | null>(null);

  const drop = useMutation({
    mutationFn: (t: string) => client!.dropTable(t),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ["schema"] }); setDropping(null); },
  });

  const role = dep.data?.role ?? "read";

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title="Schema"
        description="Table names are a flat global namespace inside this one process. There is nothing to qualify them with, and nothing outside this deployment can see them."
        actions={
          <Guarded role={role} need="admin" what="Creating a table">
            {(ok) => (
              <Button variant="primary" disabled={!ok} onClick={() => setCreating(true)}>
                <Plus aria-hidden /> New table
              </Button>
            )}
          </Guarded>
        }
      />

      <div className="space-y-3 px-5 pb-8">
        <QueryBoundary query={schema} loadingRows={8}
          unauthorized={{ have: role, need: "read", what: "The schema" }}>
          {(s) => s.tables.length === 0 ? (
            <Panel><EmptyState
              title="No tables yet"
              description="A table's engine is chosen at CREATE and never changes, so the first decision is which of the three you want."
              action={<Guarded role={role} need="admin" what="Creating a table">
                {(ok) => <Button variant="primary" disabled={!ok} onClick={() => setCreating(true)}><Plus aria-hidden /> New table</Button>}
              </Guarded>}
            /></Panel>
          ) : (
            <>
              {s.tables.map((t) => (
                <TableCard key={t.name} table={t} deploymentId={dep.data!.id} role={role} onDrop={() => setDropping(t)} />
              ))}
            </>
          )}
        </QueryBoundary>
      </div>

      {client && <CreateTableDialog open={creating} onOpenChange={setCreating} client={client} />}
      {dropping && (
        <ConfirmDialog
          open onOpenChange={(v) => !v && setDropping(null)}
          title={`Drop table ${dropping.name}`}
          confirmWord={dropping.name}
          confirmLabel="Drop table"
          busy={drop.isPending}
          onConfirm={() => drop.mutate(dropping.name)}
          blastRadius={<>
            Every fragment of <code>{dropping.name}</code> is unlinked in this deployment:{" "}
            <b>{dropping.fields.length} fields</b>,{" "}
            <b>{num(fragmentsFor(dropping.name).length)} fragments</b>,{" "}
            <b>{num(RECORD_COUNTS[dropping.name] ?? 0)} records</b>. Replicas follow the schema leader, so
            every copy drops it too.
          </>}
          detail={<>
            The pages become reclaimable but are not returned to the filesystem until a backup rewrites the file &mdash;
            backup, compaction and format migration are the same operation. Only a backup taken <i>before</i> this
            can bring the table back.
          </>}
        />
      )}
    </div>
  );
}

function TableCard({ table, deploymentId, role, onDrop }: {
  table: TableInfo; deploymentId: string; role: "read" | "write" | "admin"; onDrop: () => void;
}) {
  const frags = fragmentsFor(table.name);
  const totalBytes = frags.reduce((a, f) => a + f.bytes, 0);

  return (
    <Panel>
      <PanelHeader
        title={
          <span className="flex items-center gap-2 normal-case">
            <Link href={`/d/${deploymentId}/schema/${table.name}`} className="font-mono text-md text-fg hover:text-accent">
              {table.name}
            </Link>
            <EngineBadge engine={table.engine} />
          </span>
        }
        description={
          <span className="flex flex-wrap gap-x-4 font-mono">
            <span>{num(RECORD_COUNTS[table.name] ?? 0)} records</span>
            <span>{table.fields.length} fields</span>
            <span>{num(frags.length)} fragments</span>
            <span>{bytes(totalBytes)}</span>
          </span>
        }
        actions={
          <>
            <Button size="xs" variant="ghost" asChild>
              <Link href={`/d/${deploymentId}/schema/${table.name}`}>Fragments</Link>
            </Button>
            <Guarded role={role} need="admin" what="Dropping a table">
              {(ok) => (
                <Button size="iconSm" variant="ghost" disabled={!ok} onClick={onDrop} aria-label={`Drop ${table.name}`}>
                  <Trash2 className={ok ? "text-failed" : undefined} aria-hidden />
                </Button>
              )}
            </Guarded>
          </>
        }
      />
      <ul className="divide-y divide-line/60">
        {table.fields.map((f) => {
          const card = CARDINALITY[table.name]?.[f.name] ?? 0;
          const fieldFrags = frags.filter((x) => x.field === f.name);
          return (
            <li key={f.name} className="grid grid-cols-[minmax(120px,190px)_1fr_auto] items-center gap-3 px-3.5 py-1.5">
              <span className="truncate font-mono text-base text-fg">{f.name}</span>
              <FieldKindBadge field={f} />
              <span className="flex shrink-0 items-center gap-4 font-mono text-2xs text-fg-faint">
                <span>{card ? `${compact(card)} keys` : "no dictionary"}</span>
                <span>{fieldFrags.length} frag</span>
                <span className="w-16 text-right text-fg-muted">{bytes(fieldFrags.reduce((a, x) => a + x.bytes, 0), 0)}</span>
              </span>
            </li>
          );
        })}
      </ul>
    </Panel>
  );
}
