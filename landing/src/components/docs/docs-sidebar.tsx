import * as React from "react";
import { ChevronRight, Search, Sparkles } from "lucide-react";
import { DOCS_NAV, normalizePath, type NavItem } from "./nav-data";
import { cn } from "@/lib/cn";

/**
 * The left rail: a filter, then the tree. The page the reader is on expands to its own
 * anchors, and those anchors track the scroll position, so the rail answers both "where am
 * I in the docs" and "where am I on this page" without the reader looking twice.
 */
export function DocsSidebar({ activeId, className }: { activeId?: string; className?: string }) {
  const [q, setQ] = React.useState("");
  const input = React.useRef<HTMLInputElement>(null);
  const here = normalizePath(typeof location === "undefined" ? "" : location.pathname);

  /* ⌘K / Ctrl-K goes to the filter, because that is where every docs reader's hand goes. */
  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key.toLowerCase() === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        input.current?.focus();
        input.current?.select();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const needle = q.trim().toLowerCase();
  const groups = DOCS_NAV.map((g) => ({
    ...g,
    items: needle
      ? g.items.filter((i) =>
          [i.label, ...(i.items ?? []).map((c) => c.label)].join(" ").toLowerCase().includes(needle),
        )
      : g.items,
  })).filter((g) => g.items.length);

  return (
    <nav className={cn("pb-16 pt-5", className)} aria-label="Documentation">
      <div className="mb-6 flex items-center gap-2 px-3">
        <div className="relative min-w-0 flex-1">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-fg-faint" />
          <input
            ref={input}
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Filter docs…"
            aria-label="Filter documentation"
            className="h-8 w-full rounded border border-line bg-surface pl-8 pr-11 text-base text-fg
                       placeholder:text-fg-faint hover:border-line-strong focus:border-accent-line focus:outline-none"
          />
          <kbd className="pointer-events-none absolute right-2 top-1/2 -translate-y-1/2 font-mono text-2xs text-fg-faint">
            ⌘K
          </kbd>
        </div>
        <a
          href="https://bigdb.cloud/deployments"
          className="flex h-8 shrink-0 items-center gap-1.5 rounded border border-line px-2.5 text-base
                     text-fg-muted transition-colors duration-fast hover:border-line-strong hover:text-fg"
        >
          <Sparkles className="size-3.5 text-accent" />
          Ask
        </a>
      </div>

      {groups.map((g) => (
        <div key={g.label} className="mb-5">
          <div className="mb-1.5 flex items-center gap-2 px-3 font-mono text-2xs uppercase tracking-wide text-fg-faint">
            <g.icon className="size-3.5" />
            {g.label}
          </div>
          <ul>
            {g.items.map((item) => (
              <Row key={item.label} item={item} here={here} activeId={activeId} open={!!needle} />
            ))}
          </ul>
        </div>
      ))}

      {!groups.length && (
        <p className="px-3 text-base text-fg-faint">
          Nothing matches <code className="font-mono text-fg-muted">{q}</code>.
        </p>
      )}
    </nav>
  );
}

function Row({ item, here, activeId, open: forceOpen }: {
  item: NavItem; here: string; activeId?: string; open?: boolean;
}) {
  const onThisPage = !!item.href && normalizePath(item.href) === here;
  const [open, setOpen] = React.useState(onThisPage);
  const expanded = open || !!forceOpen;

  if (item.soon) {
    return (
      <li className="flex items-center justify-between gap-2 py-1 pl-3 pr-2 text-base text-fg-faint">
        <span>{item.label}</span>
        <span className="font-mono text-2xs uppercase tracking-wide text-fg-faint/70">soon</span>
      </li>
    );
  }

  return (
    <li>
      <div className="flex items-center">
        <a
          href={item.href}
          className={cn(
            "min-w-0 flex-1 border-l-2 py-1 pl-3 pr-2 text-base transition-colors duration-fast",
            onThisPage
              ? "border-accent font-medium text-fg"
              : "border-transparent text-fg-muted hover:border-line-strong hover:text-fg",
          )}
        >
          {item.label}
        </a>
        {item.items && (
          <button
            onClick={() => setOpen((v) => !v)}
            aria-label={expanded ? `Collapse ${item.label}` : `Expand ${item.label}`}
            aria-expanded={expanded}
            className="mr-1 shrink-0 rounded-sm p-1 text-fg-faint hover:text-fg"
          >
            <ChevronRight className={cn("size-3.5 transition-transform duration-fast", expanded && "rotate-90")} />
          </button>
        )}
      </div>

      {item.items && expanded && (
        <ul className="ml-3 border-l border-line">
          {item.items.map((c) => {
            const target = c.anchor ? `${item.href ?? ""}#${c.anchor}` : c.href;
            const on = onThisPage && c.anchor === activeId;
            return (
              <li key={c.label}>
                <a
                  href={target}
                  className={cn(
                    "-ml-px block border-l-2 py-1 pl-3 pr-2 text-base transition-colors duration-fast",
                    on ? "border-accent text-accent" : "border-transparent text-fg-faint hover:text-fg",
                  )}
                >
                  {c.label}
                </a>
              </li>
            );
          })}
        </ul>
      )}
    </li>
  );
}
