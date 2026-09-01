"use client";
import { Ban, CheckCircle2 } from "lucide-react";
import { Panel } from "@/components/ui/card";
import { Badge } from "@/components/badges/status-badge";
import { RefusalPanel } from "@/components/data/refusal-panel";
import { ErrorState, LoadingState, EmptyState } from "@/components/data/states";
import { UnauthorizedState } from "@/components/data/states";
import { PqlResultView, SqlResultTable, CopyJson } from "./result-shapes";
import { TimingStrip } from "./timing-strip";
import { isRefusal } from "@/lib/refusals";
import type { Role } from "@/lib/api/types";
import type { RunState } from "@/lib/hooks/use-workbench";

/**
 * The result pane resolves to exactly one of six states, and a refusal is one
 * of them -- shown above the previous answer rather than replacing it, because
 * the answer you already had is still true.
 */
export function ResultPane({ run, query, role, onRewrite, onNextPage }: {
  run: RunState;
  query: string;
  role: Role;
  onRewrite: (next: string) => void;
  onNextPage?: (after: number) => void;
}) {
  const unauthorized = run.error?.status === 403;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {(run.status === "refused" || (run.status === "failed" && !unauthorized)) && run.error && (
        <div className="shrink-0 border-b border-line p-3">
          {isRefusal(run.error.code)
            ? <RefusalPanel error={run.error} query={query} onRewrite={onRewrite} />
            : <Panel className="border-failed/40 bg-failed-soft/30"><ErrorState error={run.error} /></Panel>}
        </div>
      )}

      <div className="grid min-h-0 flex-1 grid-cols-1 lg:grid-cols-[minmax(0,1fr)_246px]">
        <div className="flex min-h-0 min-w-0 flex-col border-r border-line">
          <ResultHeader run={run} />
          <div className="min-h-0 flex-1 overflow-hidden">
            {run.status === "running" && !run.result && <LoadingState rows={8} />}
            {unauthorized && <UnauthorizedState have={role} need="admin" what="This statement" />}
            {!run.result && run.status !== "running" && !unauthorized && (
              run.status === "refused" ? (
                <EmptyState
                  title="No answer, because the question does not exist here"
                  description="The refusal above says what does. Nothing was run, and nothing changed — fix the construct and run again."
                  icon={Ban}
                />
              ) : (
                <EmptyState
                  title="Nothing run yet"
                  description="Write a statement and press Cmd+Enter. Autocomplete comes from GET /schema, so it will only ever suggest something this deployment can answer."
                />
              )
            )}
            {run.result && (
              run.result.kind === "sql"
                ? <SqlResultTable result={run.result.result} />
                : <PqlResultView result={run.result.result} onNextPage={onNextPage} />
            )}
          </div>
        </div>

        <div className="min-h-0 overflow-y-auto p-3">
          {run.timing
            ? <TimingStrip timing={run.timing} />
            : <p className="text-sm leading-relaxed text-fg-faint">
                Timing appears here after a run: parse, plan, the fan-out to each shard, and the merge.
              </p>}
        </div>
      </div>
    </div>
  );
}

function ResultHeader({ run }: { run: RunState }) {
  const stale = (run.status === "refused" || run.status === "failed") && !!run.result;
  return (
    <div className="flex h-8 shrink-0 items-center gap-2 border-b border-line bg-surface-sunken px-3">
      <span className="text-2xs font-medium uppercase tracking-wider text-fg-faint">result</span>
      {run.status === "ok" && <Badge tone="healthy"><CheckCircle2 className="size-2.5" aria-hidden /> 200</Badge>}
      {run.status === "refused" && <Badge tone="refused"><Ban className="size-2.5" aria-hidden /> refused</Badge>}
      {stale && <span className="text-2xs text-fg-faint">showing the previous answer</span>}
      {run.result && <span className="ml-auto"><CopyJson value={run.result.result} /></span>}
    </div>
  );
}
