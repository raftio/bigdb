"use client";
import * as React from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Dialog, DialogContent, DialogBody, DialogFooter, DialogClose } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input, Field } from "@/components/ui/input";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";
import { EngineBadge } from "@/components/badges/engine-badge";
import { cn } from "@/lib/cn";
import type { BigClient } from "@/lib/api/client";
import type { FieldKind, TableEngine } from "@/lib/api/types";

const ENGINES: Array<{ id: TableEngine; blurb: string }> = [
  { id: "bitmap", blurb: "One bit per fact. Filters and counts are free; stored values cannot be read back." },
  { id: "bitmap+columnar", blurb: "Writes both. Filters stay free and values can be aggregated. Costs both on every write." },
  { id: "columnar", blurb: "Values only. Aggregates read the column; a filter has no bitmap and must scan." },
];

/** The engine is chosen once, at CREATE, and never changes. The dialog says so. */
export function CreateTableDialog({ open, onOpenChange, client }: {
  open: boolean; onOpenChange: (v: boolean) => void; client: BigClient;
}) {
  const [name, setName] = React.useState("");
  const [engine, setEngine] = React.useState<TableEngine>("bitmap");
  const qc = useQueryClient();

  const create = useMutation({
    mutationFn: () => client.createTable(name, engine),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ["schema"] }); onOpenChange(false); setName(""); },
  });

  const valid = /^[a-z_][a-z0-9_]{0,62}$/.test(name);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title="Create table"
        description="POST /table/{t}?engine=… — table names are a flat global namespace inside this deployment.">
        <DialogBody className="space-y-4">
          <Field label="Name" htmlFor="t-name" hint="Lowercase, digits and underscores. There are no schemas or namespaces to qualify it with.">
            <Input id="t-name" value={name} onChange={(e) => setName(e.target.value)} className="font-mono"
              placeholder="orders" autoFocus autoComplete="off" spellCheck={false} aria-invalid={!!name && !valid} />
          </Field>
          <Field label="Engine" hint="Fixed at creation. Changing it later means copying the table into a new one.">
            <div className="space-y-1.5">
              {ENGINES.map((e) => (
                <button key={e.id} type="button" onClick={() => setEngine(e.id)} aria-pressed={engine === e.id}
                  className={cn("flex w-full items-start gap-2.5 rounded border px-2.5 py-2 text-left transition-colors duration-fast",
                    engine === e.id ? "border-accent bg-accent-soft/50" : "border-line hover:border-line-strong")}>
                  <EngineBadge engine={e.id} />
                  <span className="text-sm leading-relaxed text-fg-muted">{e.blurb}</span>
                </button>
              ))}
            </div>
          </Field>
        </DialogBody>
        <DialogFooter>
          <DialogClose asChild><Button variant="ghost">Cancel</Button></DialogClose>
          <Button variant="primary" disabled={!valid || create.isPending} onClick={() => create.mutate()}>
            {create.isPending ? "Creating…" : "Create table"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

const KINDS: Array<{ id: FieldKind; hint: string }> = [
  { id: "set", hint: "many values per record" },
  { id: "mutex", hint: "one value per record" },
  { id: "bool", hint: "two rows" },
  { id: "int", hint: "bit-sliced, unsigned" },
  { id: "signed_int", hint: "bit-sliced, biased" },
  { id: "decimal", hint: "bit-sliced + scale" },
  { id: "time_quantum", hint: "one view per granularity" },
];

export function CreateFieldDialog({ open, onOpenChange, client, table }: {
  open: boolean; onOpenChange: (v: boolean) => void; client: BigClient; table: string;
}) {
  const [name, setName] = React.useState("");
  const [kind, setKind] = React.useState<FieldKind>("set");
  const [bitDepth, setBitDepth] = React.useState(32);
  const [scale, setScale] = React.useState(2);
  const qc = useQueryClient();

  const bsi = kind === "int" || kind === "signed_int" || kind === "decimal";
  const create = useMutation({
    mutationFn: () => client.createField(table, name, {
      kind, bit_depth: bsi ? bitDepth : undefined, scale: kind === "decimal" ? scale : undefined,
    }),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ["schema"] }); onOpenChange(false); setName(""); },
  });

  const valid = /^[a-z_][a-z0-9_]{0,62}$/.test(name);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={`Create field on ${table}`}
        description="POST /table/{t}/field/{f}?kind=&bit_depth=&scale= — a kind may be a set, a mutex or a time quantum, which is why SQL has no type name for it.">
        <DialogBody className="space-y-4">
          <Field label="Name" htmlFor="f-name">
            <Input id="f-name" value={name} onChange={(e) => setName(e.target.value)} className="font-mono"
              placeholder="country" autoFocus autoComplete="off" spellCheck={false} aria-invalid={!!name && !valid} />
          </Field>
          <Field label="Kind">
            <Select value={kind} onValueChange={(v) => setKind(v as FieldKind)}>
              <SelectTrigger className="w-full font-mono"><SelectValue /></SelectTrigger>
              <SelectContent>
                {KINDS.map((k) => <SelectItem key={k.id} value={k.id} hint={k.hint}>{k.id}</SelectItem>)}
              </SelectContent>
            </Select>
          </Field>
          {bsi && (
            <Field label="Bit depth" hint="Bit planes per value. Every plane is one more bitmap to AND during a range query.">
              <Input type="number" min={1} max={64} value={bitDepth} className="font-mono"
                onChange={(e) => setBitDepth(Number(e.target.value))} />
            </Field>
          )}
          {kind === "decimal" && (
            <Field label="Scale" hint="Digits after the point. A comparison written with more digits is refused, never rounded — rounding would answer a different question.">
              <Input type="number" min={0} max={8} value={scale} className="font-mono"
                onChange={(e) => setScale(Number(e.target.value))} />
            </Field>
          )}
        </DialogBody>
        <DialogFooter>
          <DialogClose asChild><Button variant="ghost">Cancel</Button></DialogClose>
          <Button variant="primary" disabled={!valid || create.isPending} onClick={() => create.mutate()}>
            {create.isPending ? "Creating…" : "Create field"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
