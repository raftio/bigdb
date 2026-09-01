"use client";
import * as S from "@radix-ui/react-separator";
import { cn } from "@/lib/cn";

export function Separator({ className, orientation = "horizontal" }: {
  className?: string; orientation?: "horizontal" | "vertical";
}) {
  return (
    <S.Root
      decorative
      orientation={orientation}
      className={cn("bg-line shrink-0", orientation === "horizontal" ? "h-px w-full" : "w-px self-stretch", className)}
    />
  );
}
