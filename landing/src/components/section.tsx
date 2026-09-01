import { cn } from "@/lib/cn";

/**
 * One scaffolding rule for every marketing section, so no band invents its own
 * rhythm. A band is separated by a hairline, never by a background change —
 * the page is one surface with lines drawn on it.
 */
export function Band({ id, children, className }: {
  id?: string; children: React.ReactNode; className?: string;
}) {
  return (
    /* scroll-mt clears the sticky header when an anchor jumps to this band. */
    <section id={id} className={cn("scroll-mt-14 border-t border-line py-14 md:py-20 lg:py-24", className)}>
      <div className="mx-auto max-w-site px-5 md:px-8 lg:px-12">{children}</div>
    </section>
  );
}

export function BandHead({ title, lede, aside }: {
  title: React.ReactNode; lede?: React.ReactNode; aside?: React.ReactNode;
}) {
  return (
    <div className="mb-8 flex flex-wrap items-baseline gap-x-7 gap-y-3">
      <h2 className="text-2xl font-medium tracking-tight text-fg md:text-[32px] md:leading-[1.12]">{title}</h2>
      {lede && <p className="max-w-[52ch] text-md text-fg-muted">{lede}</p>}
      {aside && <div className="ml-auto">{aside}</div>}
    </div>
  );
}

/** The mark: two set bits, two unset. */
export function Mark({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" aria-hidden className="shrink-0">
      <rect x="0" y="0" width="7" height="7" rx="1" fill="hsl(var(--accent))" />
      <rect x="9" y="0" width="7" height="7" rx="1" fill="hsl(var(--unset))" />
      <rect x="0" y="9" width="7" height="7" rx="1" fill="hsl(var(--unset))" />
      <rect x="9" y="9" width="7" height="7" rx="1" fill="hsl(var(--accent))" />
    </svg>
  );
}
