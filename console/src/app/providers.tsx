"use client";
import * as React from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { TooltipProvider } from "@/components/ui/tooltip";
import { CommandPaletteProvider } from "@/components/shell/command-palette";

export function Providers({ children }: { children: React.ReactNode }) {
  const [qc] = React.useState(() => new QueryClient({
    defaultOptions: {
      queries: { staleTime: 15_000, retry: (n, e: any) => (e?.status === 403 || e?.status === 404 ? false : n < 1), refetchOnWindowFocus: false },
    },
  }));
  return (
    <QueryClientProvider client={qc}>
      <TooltipProvider delayDuration={200} skipDelayDuration={300}>
        <CommandPaletteProvider>{children}</CommandPaletteProvider>
      </TooltipProvider>
    </QueryClientProvider>
  );
}
