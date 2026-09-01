import { cn } from "@/lib/cn";

export function Panel({ className, children, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cn("rounded-lg border border-line bg-surface", className)} {...props}>
      {children}
    </div>
  );
}

export function PanelHeader({ title, description, actions, className }: {
  title: React.ReactNode; description?: React.ReactNode; actions?: React.ReactNode; className?: string;
}) {
  return (
    <div className={cn("flex items-start justify-between gap-4 border-b border-line px-3.5 py-2.5", className)}>
      <div className="min-w-0">
        <h2 className="truncate text-sm font-semibold tracking-wide text-fg-muted uppercase">{title}</h2>
        {description && <p className="mt-0.5 text-xs text-fg-faint">{description}</p>}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-1.5">{actions}</div>}
    </div>
  );
}

export function SectionTitle({ children, className }: { children: React.ReactNode; className?: string }) {
  return <h2 className={cn("text-sm font-semibold uppercase tracking-wide text-fg-muted", className)}>{children}</h2>;
}
