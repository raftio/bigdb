import * as React from "react";
import { cn } from "@/lib/cn";

/**
 * The type scale for a page of prose, in one place. A docs page is read, not scanned like a
 * marketing band, so the measure is capped near 70 characters and every heading carries the
 * id the two rails point at.
 */

export function DocHeader({ crumbs, tags, title, lede, children }: {
  crumbs: { label: string; href?: string }[];
  tags?: string[];
  title: string;
  lede: React.ReactNode;
  children?: React.ReactNode;
}) {
  return (
    <header className="border-b border-line pb-8 pt-9 md:pt-12">
      <nav className="mb-4 flex flex-wrap items-center gap-x-2 gap-y-1 font-mono text-xs text-fg-faint" aria-label="Breadcrumb">
        {crumbs.map((c, i) => (
          <React.Fragment key={c.label}>
            {i > 0 && <span aria-hidden>/</span>}
            {c.href ? <a href={c.href} className="hover:text-fg">{c.label}</a> : <span className="text-fg-muted">{c.label}</span>}
          </React.Fragment>
        ))}
      </nav>

      <h1 className="max-w-[20ch] text-3xl font-medium tracking-tight text-fg md:text-[42px] md:leading-[1.08]">
        {title}
      </h1>

      {tags && (
        <div className="mt-4 flex flex-wrap gap-1.5">
          {tags.map((t) => (
            <span key={t} className="rounded-sm border border-line bg-surface px-2 py-0.5 font-mono text-2xs uppercase tracking-wide text-fg-muted">
              {t}
            </span>
          ))}
        </div>
      )}

      <p className="mt-5 max-w-[68ch] text-lg leading-relaxed text-fg-muted">{lede}</p>
      {children}
    </header>
  );
}

/** A plain section: an id for the rails, a heading, an optional lede. */
export function DocSection({ id, title, lede, children }: {
  id: string; title: string; lede?: React.ReactNode; children: React.ReactNode;
}) {
  return (
    <section id={id} className="scroll-mt-24 border-b border-line py-10 last:border-0 md:py-12">
      <h2 className="text-xl font-medium tracking-tight text-fg md:text-2xl">{title}</h2>
      {lede && <p className="mt-3 max-w-[70ch] text-md leading-relaxed text-fg-muted">{lede}</p>}
      <div className="mt-6">{children}</div>
    </section>
  );
}

/**
 * A numbered step. The number is a marker in the margin rather than part of the heading, so
 * the sequence reads down the left edge and a heading is still just its own words.
 */
export function DocStep({ id, n, title, lede, children }: {
  id: string; n: number; title: string; lede?: React.ReactNode; children: React.ReactNode;
}) {
  return (
    <section id={id} className="scroll-mt-24 border-b border-line py-10 last:border-0 md:py-12">
      <div className="md:grid md:grid-cols-[34px_minmax(0,1fr)] md:gap-x-5">
        <div
          aria-hidden
          className="mb-3 flex size-[26px] items-center justify-center rounded-full border border-accent-line
                     bg-accent-soft font-mono text-xs text-accent md:mb-0 md:mt-0.5"
        >
          {n}
        </div>
        <div className="min-w-0">
          <h2 className="text-xl font-medium tracking-tight text-fg md:text-2xl">
            <span className="sr-only">Step {n}. </span>
            {title}
          </h2>
          {lede && <p className="mt-3 max-w-[70ch] text-md leading-relaxed text-fg-muted">{lede}</p>}
          <div className="mt-6">{children}</div>
        </div>
      </div>
    </section>
  );
}

/** Body copy, at the measure the whole page is set to. */
export function P({ children, className }: { children: React.ReactNode; className?: string }) {
  return <p className={cn("mb-4 max-w-[70ch] text-md leading-relaxed text-fg-muted last:mb-0", className)}>{children}</p>;
}

/** The line under a snippet: what the reader is looking at, smaller than the prose. */
export function Caption({ children }: { children: React.ReactNode }) {
  return <p className="mb-5 max-w-[68ch] px-0.5 text-base leading-relaxed text-fg-faint">{children}</p>;
}

/** An aside that is not a warning — a fact worth pulling out of the flow. */
export function Note({ title, tone = "accent", children }: {
  title?: string; tone?: "accent" | "refused"; children: React.ReactNode;
}) {
  return (
    <div className={cn(
      "my-6 rounded border-l-2 bg-surface px-4 py-3",
      tone === "refused" ? "border-refused/60" : "border-accent-line",
    )}>
      {title && (
        <div className={cn(
          "mb-1 font-mono text-2xs uppercase tracking-wide",
          tone === "refused" ? "text-refused" : "text-accent",
        )}>
          {title}
        </div>
      )}
      <div className="max-w-[68ch] text-base leading-relaxed text-fg-muted">{children}</div>
    </div>
  );
}

/** Inline code, everywhere, one spelling. */
export function C({ children }: { children: React.ReactNode }) {
  return (
    <code className="rounded-sm bg-surface-sunken px-1 py-px font-mono text-[0.92em] text-fg">
      {children}
    </code>
  );
}

/** Previous / next, at the foot of every page. */
export function DocPager({ prev, next }: {
  prev?: { label: string; href: string }; next?: { label: string; href: string };
}) {
  return (
    <nav className="flex flex-wrap gap-3 pt-10 print:hidden" aria-label="Pagination">
      {prev && <PagerLink dir="Previous" {...prev} />}
      {next && <PagerLink dir="Next" {...next} className="ml-auto text-right" />}
    </nav>
  );
}

function PagerLink({ dir, label, href, className }: {
  dir: string; label: string; href: string; className?: string;
}) {
  return (
    <a
      href={href}
      className={cn(
        "min-w-[180px] flex-1 rounded border border-line bg-surface px-4 py-3 transition-colors duration-fast hover:border-line-strong",
        className,
      )}
    >
      <div className="font-mono text-2xs uppercase tracking-wide text-fg-faint">{dir}</div>
      <div className="mt-0.5 text-md text-fg">{label}</div>
    </a>
  );
}
