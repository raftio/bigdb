"use client";
import { AppShell } from "@/components/shell/app-shell";
import { ConnectionBanner } from "@/components/shell/connection-banner";
import { useDeployment } from "@/lib/hooks/use-deployment";
import { ErrorState, LoadingState } from "@/components/data/states";

export default function DeploymentLayout({ children }: { children: React.ReactNode }) {
  const q = useDeployment();
  if (q.isLoading) return <div className="p-6"><LoadingState rows={8} /></div>;
  if (q.error || !q.data) return <div className="p-6"><ErrorState error={q.error} onRetry={q.refetch} /></div>;
  return (
    <AppShell deployment={q.data}>
      <ConnectionBanner deployment={q.data} onRetry={() => q.refetch()} />
      {children}
    </AppShell>
  );
}
