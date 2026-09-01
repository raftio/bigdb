"use client";
import * as React from "react";
import { useRouter } from "next/navigation";
import { useMutation } from "@tanstack/react-query";
import { Check, Copy, Loader2 } from "lucide-react";
import { controlPlane } from "@/lib/api/client";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input, Field } from "@/components/ui/input";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/badges/status-badge";
import { cn } from "@/lib/cn";

const REGIONS = [
  { id: "eu-west-1", label: "eu-west-1 · Ireland" },
  { id: "eu-central-1", label: "eu-central-1 · Frankfurt" },
  { id: "us-east-1", label: "us-east-1 · N. Virginia" },
  { id: "ap-southeast-1", label: "ap-southeast-1 · Singapore" },
];

const SIZES = [
  { id: "s1", label: "s1", hint: "2 vCPU · 8 GiB · 100 GiB file" },
  { id: "s2", label: "s2", hint: "4 vCPU · 32 GiB · 500 GiB file" },
  { id: "s4", label: "s4", hint: "16 vCPU · 128 GiB · 4 TiB file" },
];

export default function NewDeploymentPage() {
  const router = useRouter();
  const [name, setName] = React.useState("");
  const [region, setRegion] = React.useState(REGIONS[0].id);
  const [size, setSize] = React.useState("s2");
  const [cluster, setCluster] = React.useState(false);
  const [copied, setCopied] = React.useState(false);

  const create = useMutation({
    mutationFn: () => controlPlane.createDeployment({ name, region, size, cluster }),
  });

  const valid = /^[a-z][a-z0-9-]{2,30}$/.test(name);
  const token = create.data ? `big_a_${create.data.id.replace(/[^a-z0-9]/g, "")}Kq2ZmT4pR9vXbN7cLd3wYs6eHu1jFa` : "";

  if (create.data) {
    return (
      <div className="mx-auto max-w-2xl">
        <PageHeader title={<><span className="font-mono">{create.data.name}</span> is up</>}
          description="One bigd process over one fresh file. It has no tables yet — a table's engine is chosen at CREATE and never changes." />
        <div className="space-y-4 px-5 pb-8">
          <Panel className="border-degraded/40">
            <PanelHeader title="Admin token" description="Shown once. The control plane keeps a prefix, never the secret." />
            <div className="space-y-3 p-3.5">
              <div className="flex items-center gap-2 rounded border border-line bg-surface-sunken px-2.5 py-2">
                <code className="min-w-0 flex-1 truncate text-sm text-fg">{token}</code>
                <Button size="xs" variant={copied ? "primary" : "default"}
                  onClick={() => { navigator.clipboard?.writeText(token); setCopied(true); setTimeout(() => setCopied(false), 1500); }}>
                  {copied ? <><Check aria-hidden /> Copied</> : <><Copy aria-hidden /> Copy</>}
                </Button>
              </div>
              <p className="text-base leading-relaxed text-fg-muted">
                Bearer tokens come from a file the process reads at start. Roles are verbs:
                {" "}<code>read</code>, <code>write</code>, <code>admin</code>. Issue narrower ones in <b>Access</b>
                {" "}rather than sharing this.
              </p>
            </div>
          </Panel>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={() => router.push("/deployments")}>All deployments</Button>
            <Button variant="primary" onClick={() => router.push(`/d/${create.data!.id}/schema`)}>Create the first table</Button>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-2xl">
      <PageHeader title="New deployment"
        description="One process, one file, one tenant. Everything inside it — tables, tokens, ranges — belongs to it alone." />
      <form className="space-y-4 px-5 pb-8" onSubmit={(e) => { e.preventDefault(); if (valid) create.mutate(); }}>
        <Panel>
          <div className="space-y-4 p-4">
            <Field label="Name" htmlFor="dep-name"
              hint={<>Lowercase letters, digits and hyphens; 3–31 characters. It becomes the hostname. {name && !valid && <span className="text-failed">That name does not match.</span>}</>}>
              <Input id="dep-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="events-prod"
                className="font-mono" autoFocus autoComplete="off" spellCheck={false} aria-invalid={!!name && !valid} />
            </Field>

            <Field label="Region">
              <Select value={region} onValueChange={setRegion}>
                <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>{REGIONS.map((r) => <SelectItem key={r.id} value={r.id}>{r.label}</SelectItem>)}</SelectContent>
              </Select>
            </Field>

            <Field label="Size" hint="Sizing is memory and file ceiling. The one allocation that grows with cardinality is the row-key dictionary; watch it on the overview.">
              <div className="grid grid-cols-3 gap-2">
                {SIZES.map((s) => (
                  <button key={s.id} type="button" onClick={() => setSize(s.id)}
                    aria-pressed={size === s.id}
                    className={cn(
                      "rounded border px-2.5 py-2 text-left transition-colors duration-fast",
                      size === s.id ? "border-accent bg-accent-soft/60" : "border-line bg-surface hover:border-line-strong",
                    )}>
                    <div className="font-mono text-base text-fg">{s.label}</div>
                    <div className="mt-0.5 text-xs leading-snug text-fg-faint">{s.hint}</div>
                  </button>
                ))}
              </div>
            </Field>
          </div>
        </Panel>

        <Panel>
          <div className="flex items-start gap-3 p-4">
            <Switch checked={cluster} onCheckedChange={setCluster} id="cluster" aria-describedby="cluster-hint" />
            <div className="min-w-0">
              <label htmlFor="cluster" className="block text-base font-medium text-fg">Run as a cluster</label>
              <p id="cluster-hint" className="mt-1 text-base leading-relaxed text-fg-muted">
                Ranges come from a config file and one copy serves each range. Failover is about a second, and one
                schema leader owns every row key. A read is never answered from a copy known to be stale — if the owner
                cannot answer, the request is refused with the shards it holds, not served partial.
              </p>
              <div className="mt-2 flex flex-wrap gap-1.5">
                <Badge tone="accent">CP</Badge>
                <Badge>failover ≈ 1s</Badge>
                <Badge>one schema leader</Badge>
                <Badge>no stale reads</Badge>
              </div>
            </div>
          </div>
        </Panel>

        <div className="flex items-center justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => router.back()}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={!valid || create.isPending}>
            {create.isPending && <Loader2 className="animate-spin" aria-hidden />}
            {create.isPending ? "Starting bigd…" : "Create deployment"}
          </Button>
        </div>
      </form>
    </div>
  );
}
