"use client";
import * as React from "react";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Check, Copy, KeyRound, Plus, ShieldAlert } from "lucide-react";
import { useClient, useDeployment } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input, Field } from "@/components/ui/input";
import { Dialog, DialogContent, DialogBody, DialogFooter, DialogClose } from "@/components/ui/dialog";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";
import { RoleBadge } from "@/components/badges/role-badge";
import { Badge } from "@/components/badges/status-badge";
import { DataTable, type Column } from "@/components/data/data-table";
import { QueryBoundary, EmptyState } from "@/components/data/states";
import { Guarded } from "@/components/data/guarded";
import { ConfirmDialog } from "@/components/data/confirm-dialog";
import { absTime, relTime } from "@/lib/format";
import type { Role, Token } from "@/lib/api/types";

/** Bearer tokens from a file. Roles are verbs: read, write, admin. */
export default function AccessPage() {
  const dep = useDeployment();
  const client = useClient();
  const qc = useQueryClient();
  const [creating, setCreating] = React.useState(false);
  const [name, setName] = React.useState("");
  const [role, setRole] = React.useState<Role>("read");
  const [revealed, setRevealed] = React.useState<Token | null>(null);
  const [copied, setCopied] = React.useState(false);
  const [revoking, setRevoking] = React.useState<Token | null>(null);

  const tokens = useQuery({
    queryKey: ["tokens", dep.data?.id], queryFn: () => client!.tokens(), enabled: !!client,
  });
  const create = useMutation({
    mutationFn: () => client!.createToken(name, role),
    onSuccess: (t) => { setRevealed(t); setCreating(false); setName(""); qc.invalidateQueries({ queryKey: ["tokens"] }); },
  });
  const revoke = useMutation({
    mutationFn: (id: string) => client!.revokeToken(id),
    onSuccess: () => { qc.invalidateQueries({ queryKey: ["tokens"] }); setRevoking(null); },
  });

  const myRole = dep.data?.role ?? "read";

  const columns: Column<Token>[] = [
    { key: "name", header: "name", width: 190, mono: true,
      cell: (t) => <span className={t.revoked_at ? "text-fg-faint line-through" : "text-fg"}>{t.name}</span>,
      sort: (a, b) => a.name.localeCompare(b.name) },
    { key: "prefix", header: "prefix", width: 120, mono: true,
      cell: (t) => <span className="text-fg-muted">{t.prefix}&hellip;</span> },
    { key: "role", header: "role", width: 80, cell: (t) => <RoleBadge role={t.role} /> },
    { key: "created", header: "created", width: 190, mono: true,
      cell: (t) => <span className="text-fg-muted">{absTime(t.created_at)}</span>,
      sort: (a, b) => a.created_at.localeCompare(b.created_at) },
    { key: "used", header: "last used", width: 120, mono: true,
      cell: (t) => t.last_used_at
        ? <span className="text-fg-muted">{relTime(t.last_used_at)}</span>
        : <span className="text-degraded">never</span>,
      sort: (a, b) => (a.last_used_at ?? "").localeCompare(b.last_used_at ?? "") },
    { key: "status", header: "status", width: 110,
      cell: (t) => t.revoked_at ? <Badge tone="failed">revoked</Badge> : <Badge tone="healthy">active</Badge> },
    { key: "actions", header: "", width: 84, align: "right",
      cell: (t) => t.revoked_at ? null : (
        <Guarded role={myRole} need="admin" what="Revoking a token">
          {(ok) => <Button size="xs" variant="ghost" disabled={!ok} onClick={() => setRevoking(t)}
            className={ok ? "text-failed" : undefined}>Revoke</Button>}
        </Guarded>
      ) },
  ];

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title="Access"
        description={<>Bearer tokens, read from a file the process loads at start. Every token is scoped to this one deployment &mdash; there is nothing cross-deployment to scope to, because a deployment is a process.</>}
        actions={
          <Guarded role={myRole} need="admin" what="Issuing a token">
            {(ok) => <Button variant="primary" disabled={!ok} onClick={() => setCreating(true)}><Plus aria-hidden /> New token</Button>}
          </Guarded>
        }
      />

      <div className="grid grid-cols-1 gap-4 px-5 pb-8 xl:grid-cols-[minmax(0,1fr)_360px]">
        <Panel className="min-w-0 overflow-hidden">
          <PanelHeader title="Tokens" description="Roles are verbs, not rows: read ⊂ write ⊂ admin." />
          <QueryBoundary query={tokens} loadingRows={6}
            unauthorized={{ have: myRole, need: "admin", what: "Token management" }}>
            {(rows) => (
              <DataTable rows={rows} columns={columns} rowKey={(t) => t.id}
                empty={<EmptyState title="No tokens" icon={KeyRound}
                  description="Without a token every authenticated route answers 401. /health and /ready stay open." />} />
            )}
          </QueryBoundary>
        </Panel>

        <div className="space-y-4">
          <Panel>
            <PanelHeader title="What each role can do" />
            <ul className="divide-y divide-line/60 text-base">
              {([
                ["read", "GET /schema, GET /metrics, GET /verify, POST /table/{t}/query, POST /sql (SELECT)"],
                ["write", "everything read can, plus POST /table/{t}/import and POST /table/{t}/delete"],
                ["admin", "everything write can, plus table and field DDL, POST /repair and POST /admin/backup"],
              ] as Array<[Role, string]>).map(([r, what]) => (
                <li key={r} className="flex gap-2.5 px-3 py-2">
                  <RoleBadge role={r} />
                  <code className="min-w-0 flex-1 text-xs leading-relaxed text-fg-muted">{what}</code>
                </li>
              ))}
            </ul>
          </Panel>

          <Panel className="border-degraded/35">
            <PanelHeader title={<span className="flex items-center gap-1.5 normal-case"><ShieldAlert className="size-3.5 text-degraded" aria-hidden /> Loopback is not a role</span>} />
            <div className="space-y-2 p-3.5 text-base leading-relaxed text-fg-muted">
              <p>
                A request from <code>127.0.0.1</code> with no <code>Authorization</code> header is refused like any
                other. There is no implicit trust for local callers, no unix-socket bypass and no first-run grace period.
              </p>
              <pre className="overflow-x-auto rounded border border-line bg-surface-sunken px-2.5 py-2 text-sm"><code>{`$ curl -s localhost:8080/schema
{"code":"unauthorized",
 "message":"missing bearer token"}   # 401`}</code></pre>
              <p className="text-xs text-fg-faint">
                Counted by <code>big_http_unauthorized_total</code>, so a spike is visible on the overview.
              </p>
            </div>
          </Panel>
        </div>
      </div>

      {/* create */}
      <Dialog open={creating} onOpenChange={setCreating}>
        <DialogContent title="New token" description="The secret is shown once, here, and never again — the control plane keeps only the prefix.">
          <DialogBody className="space-y-4">
            <Field label="Name" hint="What is going to hold it. This is the only handle you will have when deciding what to revoke.">
              <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="ingest-worker-eu"
                className="font-mono" autoFocus autoComplete="off" />
            </Field>
            <Field label="Role" hint="Pick the narrowest verb that does the job. A read token cannot be widened later; issue a new one.">
              <Select value={role} onValueChange={(v) => setRole(v as Role)}>
                <SelectTrigger className="w-full font-mono"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="read" hint="query, schema, metrics">read</SelectItem>
                  <SelectItem value="write" hint="+ import, delete">write</SelectItem>
                  <SelectItem value="admin" hint="+ DDL, repair, backup">admin</SelectItem>
                </SelectContent>
              </Select>
            </Field>
          </DialogBody>
          <DialogFooter>
            <DialogClose asChild><Button variant="ghost">Cancel</Button></DialogClose>
            <Button variant="primary" disabled={!name || create.isPending} onClick={() => create.mutate()}>
              {create.isPending ? "Issuing…" : "Issue token"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* reveal-once */}
      <Dialog open={!!revealed} onOpenChange={(v) => !v && setRevealed(null)}>
        <DialogContent title="Copy this token now"
          description="This is the only time it is shown. Close this dialog and it is unrecoverable — issue a new one instead.">
          <DialogBody className="space-y-3">
            <div className="flex items-center gap-2 rounded border border-degraded/40 bg-degraded-soft/50 px-2.5 py-2">
              <code className="min-w-0 flex-1 break-all text-sm text-fg">{revealed?.secret}</code>
              <Button size="xs" variant={copied ? "primary" : "default"}
                onClick={() => { navigator.clipboard?.writeText(revealed!.secret!); setCopied(true); setTimeout(() => setCopied(false), 1500); }}>
                {copied ? <><Check aria-hidden /> Copied</> : <><Copy aria-hidden /> Copy</>}
              </Button>
            </div>
            <div className="flex items-center gap-2 text-base text-fg-muted">
              <RoleBadge role={revealed?.role ?? "read"} />
              <span>scoped to <code>{dep.data?.name}</code></span>
            </div>
          </DialogBody>
          <DialogFooter>
            <Button variant="primary" onClick={() => setRevealed(null)}>I have copied it</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {revoking && (
        <ConfirmDialog
          open onOpenChange={(v) => !v && setRevoking(null)}
          title={`Revoke ${revoking.name}`}
          confirmWord={revoking.name}
          confirmLabel="Revoke token"
          busy={revoke.isPending}
          onConfirm={() => revoke.mutate(revoking.id)}
          blastRadius={<>
            Every request carrying <code>{revoking.prefix}&hellip;</code> starts answering <code>401</code> as soon as
            the process reloads its token file &mdash; within seconds. Anything still using it{" "}
            {revoking.last_used_at ? <>(last seen {relTime(revoking.last_used_at)})</> : <>(never used)</>} stops working.
          </>}
          detail="Revocation cannot be undone. Issue a replacement first if something depends on this token."
        />
      )}
    </div>
  );
}
