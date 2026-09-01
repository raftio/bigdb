"use client";
import * as S from "@radix-ui/react-select";
import { Check, ChevronDown } from "lucide-react";
import { cn } from "@/lib/cn";

export const Select = S.Root;
export const SelectValue = S.Value;

export function SelectTrigger({ className, children, ...props }: React.ComponentProps<typeof S.Trigger>) {
  return (
    <S.Trigger
      className={cn(
        "flex h-8 items-center justify-between gap-2 rounded border border-line bg-surface px-2.5",
        "text-base text-fg transition-colors duration-fast hover:border-line-strong",
        "data-[state=open]:border-accent disabled:opacity-45 disabled:pointer-events-none", className,
      )}
      {...props}
    >
      {children}
      <S.Icon><ChevronDown className="size-3.5 text-fg-faint" /></S.Icon>
    </S.Trigger>
  );
}

export function SelectContent({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <S.Portal>
      <S.Content
        position="popper"
        sideOffset={4}
        className={cn("z-50 min-w-[--radix-select-trigger-width] overflow-hidden rounded border border-line bg-overlay shadow-e3 animate-fade-in", className)}
      >
        <S.Viewport className="p-1">{children}</S.Viewport>
      </S.Content>
    </S.Portal>
  );
}

export function SelectItem({ value, children, hint }: { value: string; children: React.ReactNode; hint?: string }) {
  return (
    <S.Item
      value={value}
      className={cn(
        "relative flex cursor-pointer select-none items-center gap-2 rounded-sm py-1.5 pl-6 pr-2",
        "text-base text-fg outline-none data-[highlighted]:bg-surface-raised",
      )}
    >
      <S.ItemIndicator className="absolute left-1.5"><Check className="size-3" /></S.ItemIndicator>
      <S.ItemText>{children}</S.ItemText>
      {hint && <span className="ml-auto text-xs text-fg-faint">{hint}</span>}
    </S.Item>
  );
}
