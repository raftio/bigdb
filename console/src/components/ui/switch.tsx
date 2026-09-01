"use client";
import * as S from "@radix-ui/react-switch";
import { cn } from "@/lib/cn";

export function Switch({ className, ...props }: React.ComponentProps<typeof S.Root>) {
  return (
    <S.Root
      className={cn(
        "peer inline-flex h-4 w-7 shrink-0 items-center rounded-full border border-line transition-colors duration-fast",
        "data-[state=checked]:bg-accent data-[state=unchecked]:bg-surface-sunken disabled:opacity-45", className,
      )}
      {...props}
    >
      <S.Thumb className="block size-3 translate-x-0.5 rounded-full bg-fg shadow transition-transform duration-fast data-[state=checked]:translate-x-3.5 data-[state=checked]:bg-accent-fg" />
    </S.Root>
  );
}
