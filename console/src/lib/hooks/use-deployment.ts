"use client";
import { useQuery } from "@tanstack/react-query";
import { useParams } from "next/navigation";
import { BigClient, controlPlane } from "@/lib/api/client";
import * as React from "react";

export function useDeploymentId(): string {
  const p = useParams<{ id: string }>();
  return p.id;
}

export function useDeployment(id?: string) {
  const routeId = useDeploymentId();
  const key = id ?? routeId;
  return useQuery({ queryKey: ["deployment", key], queryFn: () => controlPlane.deployment(key), enabled: !!key });
}

export function useDeployments() {
  return useQuery({ queryKey: ["deployments"], queryFn: controlPlane.deployments });
}

/** The client is bound to the deployment and to the role of the token held for it. */
export function useClient() {
  const { data } = useDeployment();
  return React.useMemo(() => (data ? new BigClient(data.id, data.role) : null), [data]);
}

export function useSchema() {
  const client = useClient();
  return useQuery({
    queryKey: ["schema", client?.deploymentId],
    queryFn: () => client!.schema(),
    enabled: !!client,
  });
}

export function useMetrics() {
  const client = useClient();
  return useQuery({
    queryKey: ["metrics", client?.deploymentId],
    queryFn: () => client!.metrics(),
    enabled: !!client,
    refetchInterval: 15_000,
  });
}
