"use client";
import * as T from "@radix-ui/react-tooltip";
import { cn } from "@/lib/cn";

export const TooltipProvider = T.Provider;

export function Tooltip({ content, children, side = "top", className }: {
  content: React.ReactNode; children: React.ReactNode; side?: "top" | "bottom" | "left" | "right"; className?: string;
}) {
  if (!content) return <>{children}</>;
  return (
    <T.Root delayDuration={180}>
      <T.Trigger asChild>{children}</T.Trigger>
      <T.Portal>
        <T.Content
          side={side}
          sideOffset={6}
          className={cn(
            "z-50 max-w-[300px] rounded border border-line bg-overlay px-2 py-1.5 text-xs leading-relaxed",
            "text-fg shadow-e3 animate-fade-in", className,
          )}
        >
          {content}
        </T.Content>
      </T.Portal>
    </T.Root>
  );
}
