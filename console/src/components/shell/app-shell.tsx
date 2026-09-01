"use client";
import * as React from "react";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import {
  Activity, Boxes, CreditCard, Database, Gauge, HardDriveDownload, KeyRound,
  Network, Search, Terminal, Upload, Users,
} from "lucide-react";
import { cn } from "@/lib/cn";
import { Kbd } from "@/components/ui/kbd";
import { Button } from "@/components/ui/button";
import { ThemeToggle } from "./theme";
import { useCommandPalette } from "./command-palette";
import type { Deployment } from "@/lib/api/types";
import { DeploymentSwitcher } from "./deployment-switcher";
import { RoleBadge } from "@/components/badges/role-badge";

interface NavItem { href: string; label: string; icon: React.ComponentType<{ className?: string }>; chord?: string }

export function AppShell({ deployment, children }: { deployment?: Deployment; children: React.ReactNode }) {
  const pathname = usePathname();
  const router = useRouter();
  const { open } = useCommandPalette();
  const base = deployment ? `/d/${deployment.id}` : "";

  const nav: NavItem[] = deployment ? [
    { href: base, label: "Overview", icon: Gauge, chord: "v" },
    { href: `${base}/query`, label: "Query", icon: Terminal, chord: "q" },
    { href: `${base}/schema`, label: "Schema", icon: Boxes, chord: "s" },
    { href: `${base}/ingest`, label: "Ingest", icon: Upload, chord: "i" },
    { href: `${base}/cluster`, label: "Cluster", icon: Network, chord: "c" },
    { href: `${base}/backups`, label: "Backups", icon: HardDriveDownload, chord: "b" },
    { href: `${base}/access`, label: "Access", icon: KeyRound, chord: "a" },
    { href: `${base}/observability`, label: "Observability", icon: Activity, chord: "o" },
  ] : [];

  const account: NavItem[] = [
    { href: "/deployments", label: "Deployments", icon: Database },
    { href: "/settings/billing", label: "Billing", icon: CreditCard },
    { href: "/settings/team", label: "Team", icon: Users },
  ];

  /* `g` then a letter — the whole console is reachable without the mouse. */
  React.useEffect(() => {
    let armed = false;
    let timer: ReturnType<typeof setTimeout>;
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null;
      if (el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable || el.closest(".cm-editor"))) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      if (armed) {
        armed = false;
        clearTimeout(timer);
        const hit = nav.find((n) => n.chord === e.key.toLowerCase());
        if (hit) { e.preventDefault(); router.push(hit.href); }
        return;
      }
      if (e.key === "g") { armed = true; timer = setTimeout(() => { armed = false; }, 1_200); }
      if (e.key === "/") { e.preventDefault(); open(); }
    };
    window.addEventListener("keydown", onKey);
    return () => { window.removeEventListener("keydown", onKey); clearTimeout(timer); };
  }, [nav, router, open]);

  return (
    <div className="flex min-h-screen">
      <aside className="sticky top-0 flex h-screen w-[196px] shrink-0 flex-col border-r border-line bg-surface-sunken">
        <div className="flex h-11 shrink-0 items-center gap-2 border-b border-line px-3">
          <Link href="/deployments" className="flex items-center gap-2 rounded" aria-label="bigdb Cloud home">
            <Logo />
            <span className="text-md font-semibold tracking-tight">bigdb</span>
            <span className="text-md font-light text-fg-faint">Cloud</span>
          </Link>
        </div>

        {deployment && (
          <div className="border-b border-line p-2">
            <DeploymentSwitcher current={deployment} />
          </div>
        )}

        <nav className="min-h-0 flex-1 overflow-y-auto p-2" aria-label="Deployment">
          {deployment && (
            <ul className="space-y-0.5">
              {nav.map((n) => <NavLink key={n.href} item={n} active={pathname === n.href || (n.href !== base && pathname.startsWith(n.href))} />)}
            </ul>
          )}
          <div className="mt-4 mb-1 px-2 text-2xs font-medium uppercase tracking-wider text-fg-faint">Account</div>
          <ul className="space-y-0.5">
            {account.map((n) => <NavLink key={n.href} item={n} active={pathname.startsWith(n.href)} />)}
          </ul>
        </nav>

        <div className="shrink-0 border-t border-line p-2">
          <button
            onClick={open}
            className="flex w-full items-center gap-2 rounded border border-line bg-surface px-2 py-1.5 text-sm text-fg-faint transition-colors duration-fast hover:border-line-strong hover:text-fg-muted"
          >
            <Search className="size-3" aria-hidden />
            <span>Search</span>
            <Kbd className="ml-auto">⌘K</Kbd>
          </button>
        </div>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="sticky top-0 z-30 flex h-11 shrink-0 items-center gap-3 border-b border-line bg-bg/85 px-4 backdrop-blur">
          <Breadcrumbs deployment={deployment} pathname={pathname} />
          <div className="ml-auto flex items-center gap-2">
            {deployment && (
              <span className="flex items-center gap-1.5 text-xs text-fg-faint">
                <span>token</span><RoleBadge role={deployment.role} />
              </span>
            )}
            <ThemeToggle />
          </div>
        </header>
        <main id="main" className="min-w-0 flex-1">{children}</main>
      </div>
    </div>
  );
}

function NavLink({ item, active }: { item: NavItem; active: boolean }) {
  const Icon = item.icon;
  return (
    <li>
      <Link
        href={item.href}
        aria-current={active ? "page" : undefined}
        className={cn(
          "group flex items-center gap-2 rounded px-2 py-1.5 text-base transition-colors duration-fast",
          active ? "bg-surface text-fg shadow-e1" : "text-fg-muted hover:bg-surface hover:text-fg",
        )}
      >
        <Icon className={cn("size-3.5 shrink-0", active ? "text-accent" : "text-fg-faint")} aria-hidden />
        <span className="truncate">{item.label}</span>
        {item.chord && (
          <span className="ml-auto hidden font-mono text-2xs text-fg-faint group-hover:inline">g {item.chord}</span>
        )}
      </Link>
    </li>
  );
}

function Breadcrumbs({ deployment, pathname }: { deployment?: Deployment; pathname: string }) {
  const parts = pathname.split("/").filter(Boolean);
  const tail = deployment ? parts.slice(2) : parts;
  return (
    <nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-1.5 text-base">
      {deployment ? (
        <>
          <Link href={`/d/${deployment.id}`} className="truncate font-mono text-fg hover:text-accent">{deployment.name}</Link>
          {tail.map((p, i) => (
            <span key={i} className="flex min-w-0 items-center gap-1.5">
              <span className="text-fg-faint" aria-hidden>/</span>
              <span className="truncate font-mono text-fg-muted">{p}</span>
            </span>
          ))}
        </>
      ) : (
        <span className="font-mono text-fg-muted">{parts.join(" / ") || "deployments"}</span>
      )}
    </nav>
  );
}

function Logo() {
  /* The landing's mark: two set bits, two unset. The accent means exactly one
     thing across this product — a bit that is set. */
  return (
    <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden className="shrink-0">
      <rect x="0" y="0" width="7" height="7" rx="1" fill="hsl(var(--accent))" />
      <rect x="9" y="0" width="7" height="7" rx="1" fill="hsl(var(--unset))" />
      <rect x="0" y="9" width="7" height="7" rx="1" fill="hsl(var(--unset))" />
      <rect x="9" y="9" width="7" height="7" rx="1" fill="hsl(var(--accent))" />
    </svg>
  );
}
