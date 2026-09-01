import { cn } from "@/lib/cn";

export function Kbd({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <kbd className={cn(
      "inline-flex h-[18px] min-w-[18px] items-center justify-center rounded-sm border border-line",
      "bg-surface-sunken px-1 text-2xs font-medium text-fg-faint", className,
    )}>
      {children}
    </kbd>
  );
}
