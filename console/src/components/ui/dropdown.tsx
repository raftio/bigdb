"use client";
import * as D from "@radix-ui/react-dropdown-menu";
import { Check } from "lucide-react";
import { cn } from "@/lib/cn";

export const Dropdown = D.Root;
export const DropdownTrigger = D.Trigger;

export function DropdownContent({ children, className, align = "end" }: {
  children: React.ReactNode; className?: string; align?: "start" | "center" | "end";
}) {
  return (
    <D.Portal>
      <D.Content
        align={align}
        sideOffset={4}
        className={cn("z-50 min-w-[180px] overflow-hidden rounded border border-line bg-overlay p-1 shadow-e3 animate-fade-in", className)}
      >
        {children}
      </D.Content>
    </D.Portal>
  );
}

export function DropdownItem({ children, onSelect, disabled, danger, hint }: {
  children: React.ReactNode; onSelect?: () => void; disabled?: boolean; danger?: boolean; hint?: React.ReactNode;
}) {
  return (
    <D.Item
      disabled={disabled}
      onSelect={onSelect}
      className={cn(
        "flex cursor-pointer select-none items-center gap-2 rounded-sm px-2 py-1.5 text-base outline-none",
        "data-[highlighted]:bg-surface-raised data-[disabled]:pointer-events-none data-[disabled]:opacity-45",
        danger ? "text-failed" : "text-fg",
      )}
    >
      {children}
      {hint && <span className="ml-auto text-xs text-fg-faint">{hint}</span>}
    </D.Item>
  );
}

export function DropdownCheckItem({ children, checked, onSelect }: {
  children: React.ReactNode; checked: boolean; onSelect?: () => void;
}) {
  return (
    <D.Item
      onSelect={(e) => { e.preventDefault(); onSelect?.(); }}
      className="flex cursor-pointer select-none items-center gap-2 rounded-sm py-1.5 pl-6 pr-2 text-base text-fg outline-none data-[highlighted]:bg-surface-raised relative"
    >
      {checked && <Check className="absolute left-1.5 size-3" />}
      {children}
    </D.Item>
  );
}

export function DropdownLabel({ children }: { children: React.ReactNode }) {
  return <D.Label className="px-2 py-1 text-2xs font-medium uppercase tracking-wider text-fg-faint">{children}</D.Label>;
}
export const DropdownSeparator = () => <D.Separator className="my-1 h-px bg-line" />;
