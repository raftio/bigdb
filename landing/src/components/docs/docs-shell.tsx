import * as React from "react";
import { Menu, X } from "lucide-react";
import { DocsSidebar } from "./docs-sidebar";
import { DocsToc, type TocItem } from "./docs-toc";
import { useActiveSection } from "./use-active-section";
import { cn } from "@/lib/cn";

/**
 * Three columns: where the docs are, what the page says, where you are in it. The two rails
 * are chrome — they stick, they scroll on their own, and they leave in print. The middle
 * column is the only thing that is ever measured in characters.
 */
export function DocsShell({ toc, markdownUrl, children }: {
  toc: TocItem[]; markdownUrl: string; children: React.ReactNode;
}) {
  const active = useActiveSection(toc.map((t) => t.id));
  const [menu, setMenu] = React.useState(false);

  /* A tap on a link in the drawer is a navigation; the drawer should not survive it. */
  React.useEffect(() => {
    if (!menu) return;
    const close = (e: MouseEvent) => {
      if ((e.target as HTMLElement).closest("a")) setMenu(false);
    };
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setMenu(false);
    document.addEventListener("click", close);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("click", close);
      document.removeEventListener("keydown", esc);
    };
  }, [menu]);

  return (
    <div className="mx-auto flex w-full max-w-[1480px] items-start px-0 lg:px-6">
      <aside className="sticky top-14 hidden h-[calc(100vh-3.5rem)] w-[264px] shrink-0 overflow-y-auto
                        border-r border-line lg:block print:hidden">
        <DocsSidebar activeId={active} />
      </aside>

      <div className="min-w-0 flex-1">
        {/* The rail, collapsed, for a phone. */}
        <div className="sticky top-14 z-20 flex items-center gap-3 border-b border-line bg-bg/92 px-5 py-2
                        backdrop-blur lg:hidden print:hidden">
          <button
            onClick={() => setMenu((v) => !v)}
            className="flex items-center gap-1.5 rounded border border-line px-2 py-1 text-base text-fg-muted"
            aria-expanded={menu}
          >
            {menu ? <X className="size-3.5" /> : <Menu className="size-3.5" />}
            Docs
          </button>
          <span className="truncate font-mono text-xs text-fg-faint">
            {toc.find((t) => t.id === active)?.label}
          </span>
        </div>

        {menu && (
          <div className="border-b border-line bg-surface-sunken lg:hidden print:hidden">
            <DocsSidebar activeId={active} />
          </div>
        )}

        <article className={cn("mx-auto max-w-[820px] px-5 pb-20 md:px-9", menu && "hidden lg:block")}>
          {children}
        </article>
      </div>

      <aside className="sticky top-14 hidden h-[calc(100vh-3.5rem)] w-[236px] shrink-0 overflow-y-auto
                        px-4 xl:block print:hidden">
        <DocsToc items={toc} activeId={active} markdownUrl={markdownUrl} />
      </aside>
    </div>
  );
}
