"use client";
import * as T from "@radix-ui/react-tabs";
import { cn } from "@/lib/cn";

export const Tabs = T.Root;
export const TabsContent = T.Content;

export function TabsList({ className, children }: { className?: string; children: React.ReactNode }) {
  return <T.List className={cn("inline-flex items-center gap-0.5", className)}>{children}</T.List>;
}

export function TabsTrigger({ value, children, className }: { value: string; children: React.ReactNode; className?: string }) {
  return (
    <T.Trigger
      value={value}
      className={cn(
        "relative rounded px-2.5 py-1 text-sm font-medium text-fg-faint transition-colors duration-fast",
        "hover:text-fg data-[state=active]:bg-surface-raised data-[state=active]:text-fg",
        "data-[state=active]:shadow-e1", className,
      )}
    >
      {children}
    </T.Trigger>
  );
}
