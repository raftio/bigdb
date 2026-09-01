import { cn } from "@/lib/cn";

export function PageHeader({ title, description, actions, className, children }: {
  title: React.ReactNode; description?: React.ReactNode; actions?: React.ReactNode;
  className?: string; children?: React.ReactNode;
}) {
  return (
    <div className={cn("flex flex-wrap items-start justify-between gap-4 px-5 pb-4 pt-5", className)}>
      <div className="min-w-0">
        <h1 className="flex items-center gap-2.5 text-xl font-semibold tracking-tight text-fg">{title}</h1>
        {description && <p className="mt-1 max-w-3xl text-base leading-relaxed text-fg-muted">{description}</p>}
        {children}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}
