"use client";
import { useQuery } from "@tanstack/react-query";
import { UserPlus } from "lucide-react";
import { controlPlane } from "@/lib/api/client";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/badges/status-badge";
import { RoleBadge } from "@/components/badges/role-badge";
import { DataTable, type Column } from "@/components/data/data-table";
import { QueryBoundary } from "@/components/data/states";
import { DEPLOYMENTS } from "@/lib/api/mock/fixtures";
import { relTime } from "@/lib/format";
import type { Member } from "@/lib/api/types";

/**
 * Two different things share the word "role" here, and conflating them is the
 * easiest mistake to make: a *seat* role governs the console, a *token* role
 * governs the database. The page says so, once, plainly.
 */
export default function TeamPage() {
  const members = useQuery({ queryKey: ["members"], queryFn: controlPlane.members });

  const columns: Column<Member>[] = [
    { key: "name", header: "member", width: 200, cell: (m) => <span className="text-fg">{m.name}</span>,
      sort: (a, b) => a.name.localeCompare(b.name) },
    { key: "email", header: "email", width: 240, mono: true, cell: (m) => <span className="text-fg-muted">{m.email}</span> },
    { key: "role", header: "seat", width: 110,
      cell: (m) => <Badge tone={m.role === "owner" ? "accent" : "neutral"}>{m.role}</Badge> },
    { key: "active", header: "last active", width: 140, mono: true,
      cell: (m) => <span className="text-fg-muted">{relTime(m.last_active)}</span>,
      sort: (a, b) => a.last_active.localeCompare(b.last_active) },
    { key: "actions", header: "", align: "right",
      cell: (m) => m.role === "owner" ? null : <Button size="xs" variant="ghost">Manage</Button> },
  ];

  return (
    <div className="mx-auto max-w-[1400px]">
      <PageHeader
        title="Team"
        description="Seats govern this console. They do not govern the database — that is what a deployment's tokens are for."
        actions={<Button variant="primary"><UserPlus aria-hidden /> Invite</Button>}
      />

      <div className="grid grid-cols-1 gap-4 px-5 pb-8 xl:grid-cols-[minmax(0,1fr)_340px]">
        <Panel className="min-w-0 overflow-hidden">
          <PanelHeader title="Members" description={`${members.data?.length ?? 0} of 10 seats used`} />
          <QueryBoundary query={members} loadingRows={4}>
            {(rows) => <DataTable rows={rows} columns={columns} rowKey={(m) => m.id} />}
          </QueryBoundary>
        </Panel>

        <div className="space-y-4">
          <Panel>
            <PanelHeader title="Seats vs tokens" />
            <div className="space-y-2 p-3.5 text-base leading-relaxed text-fg-muted">
              <p>
                A <b className="text-fg">seat</b> decides who can open this console, create deployments, and issue
                tokens. It is an account-level fact.
              </p>
              <p>
                A <b className="text-fg">token</b> decides what a request to <code>bigd</code> may do:{" "}
                <RoleBadge role="read" /> <RoleBadge role="write" /> <RoleBadge role="admin" />. It is a bearer string
                in a file the process reads, scoped to one deployment, and the database knows nothing about seats.
              </p>
              <p>
                So an owner with only a <code>read</code> token still sees admin controls disabled here &mdash; the seat
                did not grant the verb.
              </p>
            </div>
          </Panel>

          <Panel>
            <PanelHeader title="Token roles held" description="What this account can currently do per deployment." />
            <ul className="divide-y divide-line/60">
              {DEPLOYMENTS.map((d) => (
                <li key={d.id} className="flex items-center gap-2 px-3 py-2">
                  <span className="min-w-0 flex-1 truncate font-mono text-base text-fg">{d.name}</span>
                  <RoleBadge role={d.role} />
                </li>
              ))}
            </ul>
          </Panel>
        </div>
      </div>
    </div>
  );
}
