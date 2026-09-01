"use client";
import * as React from "react";
import { useRouter } from "next/navigation";
import { Command, CornerDownLeft, Search } from "lucide-react";
import { Dialog, DialogTrigger } from "@/components/ui/dialog";
import * as D from "@radix-ui/react-dialog";
import { Kbd } from "@/components/ui/kbd";
import { EngineBadge } from "@/components/badges/engine-badge";
import { HealthDot } from "@/components/badges/health-dot";
import { cn } from "@/lib/cn";
import { DEPLOYMENTS, SCHEMA } from "@/lib/api/mock/fixtures";

export interface CommandItem {
  id: string;
  label: string;
  group: string;
  hint?: React.ReactNode;
  keywords?: string;
  run: () => void;
}

const Ctx = React.createContext<{ open: () => void; register: (items: CommandItem[]) => () => void }>({
  open: () => {}, register: () => () => {},
});

export const useCommandPalette = () => React.useContext(Ctx);

/** Screens contribute their own actions for as long as they are mounted. */
export function useRegisterCommands(items: CommandItem[], deps: React.DependencyList) {
  const { register } = useCommandPalette();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  React.useEffect(() => register(items), deps);
}

export function CommandPaletteProvider({ children }: { children: React.ReactNode }) {
  const [open, setOpen] = React.useState(false);
  const [q, setQ] = React.useState("");
  const [active, setActive] = React.useState(0);
  const [dynamic, setDynamic] = React.useState<CommandItem[]>([]);
  const router = useRouter();

  const register = React.useCallback((items: CommandItem[]) => {
    setDynamic((prev) => [...prev.filter((p) => !items.some((i) => i.id === p.id)), ...items]);
    return () => setDynamic((prev) => prev.filter((p) => !items.some((i) => i.id === p.id)));
  }, []);

  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") { e.preventDefault(); setOpen((v) => !v); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  React.useEffect(() => { if (open) { setQ(""); setActive(0); } }, [open]);

  const base: CommandItem[] = React.useMemo(() => [
    ...DEPLOYMENTS.map((d) => ({
      id: `dep-${d.id}`, group: "Deployments", label: d.name, keywords: `${d.region} ${d.host}`,
      hint: <span className="flex items-center gap-2"><HealthDot status={d.status} showLabel={false} /><span className="font-mono text-2xs text-fg-faint">{d.region}</span></span>,
      run: () => router.push(`/d/${d.id}`),
    })),
    ...SCHEMA.tables.map((t) => ({
      id: `tbl-${t.name}`, group: "Tables", label: t.name, keywords: t.fields.map((f) => f.name).join(" "),
      hint: <EngineBadge engine={t.engine} size="xs" />,
      run: () => router.push(`/d/${DEPLOYMENTS[0].id}/schema/${t.name}`),
    })),
    { id: "nav-query", group: "Go to", label: "Query workbench", hint: <Kbd>g q</Kbd>, run: () => router.push(`/d/${DEPLOYMENTS[0].id}/query`) },
    { id: "nav-schema", group: "Go to", label: "Schema explorer", hint: <Kbd>g s</Kbd>, run: () => router.push(`/d/${DEPLOYMENTS[0].id}/schema`) },
    { id: "nav-ingest", group: "Go to", label: "Ingest", hint: <Kbd>g i</Kbd>, run: () => router.push(`/d/${DEPLOYMENTS[0].id}/ingest`) },
    { id: "nav-cluster", group: "Go to", label: "Cluster & ranges", hint: <Kbd>g c</Kbd>, run: () => router.push(`/d/${DEPLOYMENTS[0].id}/cluster`) },
    { id: "nav-backups", group: "Go to", label: "Backups", hint: <Kbd>g b</Kbd>, run: () => router.push(`/d/${DEPLOYMENTS[0].id}/backups`) },
    { id: "nav-access", group: "Go to", label: "Access & tokens", hint: <Kbd>g a</Kbd>, run: () => router.push(`/d/${DEPLOYMENTS[0].id}/access`) },
    { id: "nav-obs", group: "Go to", label: "Observability", hint: <Kbd>g o</Kbd>, run: () => router.push(`/d/${DEPLOYMENTS[0].id}/observability`) },
    { id: "nav-deployments", group: "Go to", label: "All deployments", run: () => router.push("/deployments") },
    { id: "nav-billing", group: "Go to", label: "Billing & usage", run: () => router.push("/settings/billing") },
    { id: "nav-team", group: "Go to", label: "Team", run: () => router.push("/settings/team") },
  ], [router]);

  const all = React.useMemo(() => [...dynamic, ...base], [dynamic, base]);

  const results = React.useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return all.slice(0, 24);
    return all
      .map((i) => {
        const hay = `${i.label} ${i.group} ${i.keywords ?? ""}`.toLowerCase();
        const idx = hay.indexOf(needle);
        return idx < 0 ? null : { item: i, score: idx + (i.label.toLowerCase().startsWith(needle) ? -50 : 0) };
      })
      .filter(Boolean)
      .sort((a, b) => a!.score - b!.score)
      .slice(0, 24)
      .map((r) => r!.item);
  }, [q, all]);

  React.useEffect(() => { setActive(0); }, [q]);

  const groups = React.useMemo(() => {
    const m = new Map<string, CommandItem[]>();
    for (const r of results) { if (!m.has(r.group)) m.set(r.group, []); m.get(r.group)!.push(r); }
    return [...m.entries()];
  }, [results]);

  const flat = groups.flatMap(([, items]) => items);

  const run = (i: CommandItem) => { setOpen(false); i.run(); };

  return (
    <Ctx.Provider value={{ open: () => setOpen(true), register }}>
      {children}
      <Dialog open={open} onOpenChange={setOpen}>
        <D.Portal>
          <D.Overlay className="fixed inset-0 z-50 bg-black/55 animate-fade-in" />
          <D.Content
            aria-label="Command palette"
            className="fixed left-1/2 top-[18vh] z-50 w-full max-w-xl -translate-x-1/2 overflow-hidden rounded-lg border border-line bg-overlay shadow-e3 animate-slide-up outline-none"
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") { e.preventDefault(); setActive((a) => Math.min(a + 1, flat.length - 1)); }
              if (e.key === "ArrowUp") { e.preventDefault(); setActive((a) => Math.max(a - 1, 0)); }
              if (e.key === "Enter" && flat[active]) { e.preventDefault(); run(flat[active]); }
            }}
          >
            <D.Title className="sr-only">Command palette</D.Title>
            <div className="flex items-center gap-2 border-b border-line px-3">
              <Search className="size-3.5 shrink-0 text-fg-faint" aria-hidden />
              <input
                autoFocus value={q} onChange={(e) => setQ(e.target.value)}
                placeholder="Search deployments, tables, actions…"
                aria-label="Search commands"
                className="h-10 w-full bg-transparent text-md text-fg outline-none placeholder:text-fg-faint"
              />
              <Kbd>esc</Kbd>
            </div>
            <div className="max-h-[52vh] overflow-y-auto p-1.5" role="listbox">
              {!flat.length && <div className="px-3 py-8 text-center text-base text-fg-faint">Nothing matches <code>{q}</code>.</div>}
              {groups.map(([group, items]) => (
                <div key={group} className="mb-1">
                  <div className="px-2 py-1 text-2xs font-medium uppercase tracking-wider text-fg-faint">{group}</div>
                  {items.map((i) => {
                    const idx = flat.indexOf(i);
                    return (
                      <button
                        key={i.id} role="option" aria-selected={idx === active}
                        onMouseEnter={() => setActive(idx)} onClick={() => run(i)}
                        className={cn(
                          "flex w-full items-center gap-2 rounded-sm px-2 py-1.5 text-left text-base",
                          idx === active ? "bg-accent-soft/80 text-fg" : "text-fg-muted hover:bg-surface-raised",
                        )}
                      >
                        <span className="truncate">{i.label}</span>
                        <span className="ml-auto shrink-0">{i.hint}</span>
                      </button>
                    );
                  })}
                </div>
              ))}
            </div>
            <div className="flex items-center gap-3 border-t border-line px-3 py-1.5 text-2xs text-fg-faint">
              <span className="flex items-center gap-1"><Kbd>↑</Kbd><Kbd>↓</Kbd> navigate</span>
              <span className="flex items-center gap-1"><Kbd><CornerDownLeft className="size-2.5" /></Kbd> open</span>
              <span className="ml-auto flex items-center gap-1"><Command className="size-2.5" aria-hidden />K anywhere</span>
            </div>
          </D.Content>
        </D.Portal>
      </Dialog>
    </Ctx.Provider>
  );
}
