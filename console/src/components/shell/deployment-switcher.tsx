"use client";
import { useRouter } from "next/navigation";
import { ChevronsUpDown, Plus } from "lucide-react";
import { Dropdown, DropdownTrigger, DropdownContent, DropdownItem, DropdownLabel, DropdownSeparator } from "@/components/ui/dropdown";
import { HealthDot } from "@/components/badges/health-dot";
import { RoleBadge } from "@/components/badges/role-badge";
import type { Deployment } from "@/lib/api/types";
import { DEPLOYMENTS } from "@/lib/api/mock/fixtures";

/**
 * One tenant per process, so "which database" is always "which deployment".
 * The switcher shows health and the role of the token held for each, because
 * both change what you can do the moment you land.
 */
export function DeploymentSwitcher({ current }: { current: Deployment }) {
  const router = useRouter();
  return (
    <Dropdown>
      <DropdownTrigger asChild>
        <button className="flex w-full items-center gap-2 rounded border border-line bg-surface px-2 py-1.5 text-left transition-colors duration-fast hover:border-line-strong">
          <HealthDot status={current.status} showLabel={false} />
          <span className="min-w-0 flex-1">
            <span className="block truncate font-mono text-sm text-fg">{current.name}</span>
            <span className="block truncate font-mono text-2xs text-fg-faint">{current.region}</span>
          </span>
          <ChevronsUpDown className="size-3 shrink-0 text-fg-faint" aria-hidden />
        </button>
      </DropdownTrigger>
      <DropdownContent align="start" className="min-w-[260px]">
        <DropdownLabel>Deployments · one bigd, one file</DropdownLabel>
        {DEPLOYMENTS.map((d) => (
          <DropdownItem key={d.id} onSelect={() => router.push(`/d/${d.id}`)}>
            <HealthDot status={d.status} showLabel={false} />
            <span className="font-mono text-sm">{d.name}</span>
            <span className="ml-auto flex items-center gap-1.5">
              <span className="font-mono text-2xs text-fg-faint">{d.region}</span>
              <RoleBadge role={d.role} />
            </span>
          </DropdownItem>
        ))}
        <DropdownSeparator />
        <DropdownItem onSelect={() => router.push("/deployments/new")}>
          <Plus className="size-3" aria-hidden /> New deployment
        </DropdownItem>
      </DropdownContent>
    </Dropdown>
  );
}
