import { cn } from "@/lib/cn";
import type { Role } from "@/lib/api/types";
import { Tooltip } from "@/components/ui/tooltip";

/** Roles are verbs, not rows: read ⊂ write ⊂ admin. */
const SPEC: Record<Role, { tone: string; can: string }> = {
  read: { tone: "border-line-strong text-fg-muted bg-surface-sunken", can: "query, schema, metrics, verify" },
  write: { tone: "border-accent/40 text-accent bg-accent-soft/50", can: "read, plus import and delete" },
  admin: { tone: "border-degraded/45 text-degraded bg-degraded-soft", can: "write, plus DDL, repair and backup" },
};

export function RoleBadge({ role, className }: { role: Role; className?: string }) {
  return (
    <Tooltip content={<span>Can <span className="font-mono">{SPEC[role].can}</span>.</span>}>
      <span className={cn(
        "inline-flex h-[18px] items-center rounded-sm border px-1.5 font-mono text-2xs uppercase tracking-wider",
        SPEC[role].tone, className,
      )}>
        {role}
      </span>
    </Tooltip>
  );
}
