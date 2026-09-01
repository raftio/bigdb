import * as React from "react";
import { Check, Copy, FileDown, FileText, List } from "lucide-react";
import { cn } from "@/lib/cn";

export type TocItem = { id: string; label: string; depth?: 1 | 2 };

/**
 * The right rail: where you are on the page, then the three things a reader does with a
 * page they did not come here to read in a browser — take it, read it as source, print it.
 * Each action does the thing it names; none of them is a stub.
 */
export function DocsToc({ items, activeId, markdownUrl, className }: {
  items: TocItem[]; activeId?: string; markdownUrl: string; className?: string;
}) {
  return (
    <div className={cn("pb-16 pt-9", className)}>
      <div className="mb-3 flex items-center gap-2 font-mono text-2xs uppercase tracking-wide text-fg-faint">
        <List className="size-3.5" />
        On this page
      </div>
      <ul className="border-l border-line">
        {items.map((i) => (
          <li key={i.id}>
            <a
              href={`#${i.id}`}
              className={cn(
                "-ml-px block border-l-2 py-1 pr-2 text-base leading-snug transition-colors duration-fast",
                i.depth === 2 ? "pl-6" : "pl-3",
                i.id === activeId
                  ? "border-accent font-medium text-fg"
                  : "border-transparent text-fg-muted hover:text-fg",
              )}
            >
              {i.label}
            </a>
          </li>
        ))}
      </ul>

      <div className="mt-6 space-y-0.5 border-t border-line pt-5">
        <CopyPage url={markdownUrl} />
        <Action icon={FileText} label="View as Markdown" href={markdownUrl} />
        <Action icon={FileDown} label="Print or save as PDF" onClick={() => window.print()} />
      </div>
    </div>
  );
}

function CopyPage({ url }: { url: string }) {
  const [done, setDone] = React.useState<"idle" | "ok" | "fail">("idle");
  const copy = async () => {
    try {
      const md = await fetch(url).then((r) => {
        if (!r.ok) throw new Error(String(r.status));
        return r.text();
      });
      await navigator.clipboard.writeText(md);
      setDone("ok");
    } catch {
      setDone("fail");
    }
    setTimeout(() => setDone("idle"), 1600);
  };
  return (
    <Action
      icon={done === "ok" ? Check : Copy}
      label={done === "ok" ? "Copied as Markdown" : done === "fail" ? "Copy failed — open it instead" : "Copy page"}
      onClick={copy}
      tone={done === "ok" ? "accent" : done === "fail" ? "muted" : undefined}
    />
  );
}

function Action({ icon: Icon, label, href, onClick, tone }: {
  icon: React.ElementType; label: string; href?: string; onClick?: () => void; tone?: "accent" | "muted";
}) {
  const cls = cn(
    "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-base transition-colors duration-fast",
    tone === "accent" ? "text-accent" : "text-fg-muted hover:bg-surface hover:text-fg",
  );
  const body = (
    <>
      <Icon className="size-3.5 shrink-0" />
      {label}
    </>
  );
  return href ? (
    <a className={cls} href={href} target="_blank" rel="noreferrer">{body}</a>
  ) : (
    <button className={cls} onClick={onClick}>{body}</button>
  );
}
