"use client";
import * as React from "react";
import { Link2, Play, Square, History } from "lucide-react";
import { useDeployment, useSchema } from "@/lib/hooks/use-deployment";
import { useWorkbench } from "@/lib/hooks/use-workbench";
import { useRegisterCommands } from "@/components/shell/command-palette";
import { QueryEditor } from "@/components/editor/query-editor";
import { makeSqlCompletion, makePqlCompletion } from "@/components/editor/completions";
import { ResultPane } from "@/components/workbench/result-pane";
import { HistoryPanel } from "@/components/workbench/history-panel";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Button } from "@/components/ui/button";
import { Kbd } from "@/components/ui/kbd";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";
import { EngineBadge } from "@/components/badges/engine-badge";
import { LoadingState } from "@/components/data/states";
import { Tooltip } from "@/components/ui/tooltip";
import { cn } from "@/lib/cn";

/** Split editor / results, two surfaces on one editor, one run button. */
export default function QueryPage() {
  const dep = useDeployment();
  const schema = useSchema();
  const [showHistory, setShowHistory] = React.useState(true);
  const [copied, setCopied] = React.useState(false);

  if (!dep.data) return <div className="p-6"><LoadingState rows={10} /></div>;
  return <Workbench key={dep.data.id} deployment={dep.data} schema={schema.data} showHistory={showHistory}
    onToggleHistory={() => setShowHistory((v) => !v)} copied={copied} setCopied={setCopied} />;
}

function Workbench({ deployment, schema, showHistory, onToggleHistory, copied, setCopied }: {
  deployment: NonNullable<ReturnType<typeof useDeployment>["data"]>;
  schema: ReturnType<typeof useSchema>["data"];
  showHistory: boolean; onToggleHistory: () => void;
  copied: boolean; setCopied: (v: boolean) => void;
}) {
  const w = useWorkbench(deployment);
  const running = w.run.status === "running";

  const completion = React.useMemo(
    () => (w.surface === "sql" ? makeSqlCompletion(schema) : makePqlCompletion(schema, w.table)),
    [w.surface, schema, w.table],
  );

  const currentTable = schema?.tables.find((t) => t.name === w.table);

  useRegisterCommands([
    { id: "wb-run", group: "Query", label: "Run query", hint: <Kbd>⌘⏎</Kbd>, run: () => w.execute() },
    { id: "wb-sql", group: "Query", label: "Switch to SQL", run: () => w.setSurface("sql") },
    { id: "wb-pql", group: "Query", label: "Switch to PQL", run: () => w.setSurface("pql") },
    { id: "wb-link", group: "Query", label: "Copy permalink", run: () => navigator.clipboard?.writeText(w.permalink()) },
  ], [w.execute, w.setSurface, w.permalink]);

  return (
    <div className="grid h-[calc(100vh-2.75rem)] grid-cols-[minmax(0,1fr)] lg:grid-cols-[minmax(0,1fr)_auto]">
      <div className="grid min-h-0 grid-rows-[minmax(180px,34%)_minmax(0,1fr)]">
        {/* ── editor ─────────────────────────────────────────────────── */}
        <section className="flex min-h-0 flex-col border-b border-line" aria-label="Query editor">
          <div className="flex h-9 shrink-0 items-center gap-2 border-b border-line bg-surface-sunken px-2">
            <Tabs value={w.surface} onValueChange={(v) => w.setSurface(v as "sql" | "pql")}>
              <TabsList>
                <TabsTrigger value="sql">SQL</TabsTrigger>
                <TabsTrigger value="pql">PQL</TabsTrigger>
              </TabsList>
            </Tabs>

            {w.surface === "pql" && (
              <>
                <span className="text-2xs uppercase tracking-wider text-fg-faint">table</span>
                <Select value={w.table} onValueChange={w.setTable}>
                  <SelectTrigger className="h-7 min-w-[168px] font-mono text-sm"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    {schema?.tables.map((t) => (
                      <SelectItem key={t.name} value={t.name} hint={t.engine}>{t.name}</SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                {currentTable && <EngineBadge engine={currentTable.engine} />}
                <label className="flex items-center gap-1 text-2xs text-fg-faint">
                  limit
                  <input type="number" min={1} max={10_000} value={w.limit}
                    onChange={(e) => w.setLimit(Math.max(1, Number(e.target.value) || 1))}
                    className="h-6 w-16 rounded-sm border border-line bg-surface px-1 font-mono text-sm text-fg" />
                </label>
              </>
            )}
            {w.surface === "sql" && (
              <span className="text-2xs text-fg-faint">
                single-table <code>SELECT</code> and <code>CREATE TABLE</code> only &mdash; the planner is shared with PQL
              </span>
            )}

            <div className="ml-auto flex items-center gap-1.5">
              <Tooltip content={copied ? "Copied" : "Copy a link that carries the whole query"}>
                <Button size="iconSm" variant="ghost" aria-label="Copy permalink"
                  onClick={() => { navigator.clipboard?.writeText(w.permalink()); setCopied(true); setTimeout(() => setCopied(false), 1200); }}>
                  <Link2 className={cn(copied && "text-healthy")} aria-hidden />
                </Button>
              </Tooltip>
              <Tooltip content={showHistory ? "Hide history" : "Show history"}>
                <Button size="iconSm" variant="ghost" aria-label="Toggle history" aria-pressed={showHistory}
                  onClick={onToggleHistory}><History aria-hidden /></Button>
              </Tooltip>
              {running ? (
                <Button size="sm" variant="dangerOutline" onClick={w.cancel}>
                  <Square aria-hidden /> Cancel
                </Button>
              ) : (
                <Button size="sm" variant="primary" onClick={() => w.execute()}>
                  <Play aria-hidden /> Run <Kbd className="ml-1 border-accent-fg/25 bg-accent-fg/10 text-accent-fg">⌘⏎</Kbd>
                </Button>
              )}
            </div>
          </div>

          <div className="min-h-0 flex-1 bg-surface">
            <QueryEditor
              value={w.text}
              onChange={w.setText}
              language={w.surface}
              completion={completion}
              refusedSpan={w.refusedSpan}
              onRun={() => w.execute()}
              placeholder={w.surface === "sql"
                ? "SELECT country, count(*) FROM events GROUP BY country"
                : 'Count(Row(country="GB"))'}
            />
          </div>

          {running && (
            <div className="flex h-5 shrink-0 items-center gap-2 border-t border-line bg-surface-sunken px-3 text-2xs text-fg-faint">
              <span className="size-1.5 animate-pulse-dot rounded-full bg-accent" aria-hidden />
              running &mdash; Cancel closes the connection, which is what the server watches for the deadline
            </div>
          )}
        </section>

        {/* ── results ────────────────────────────────────────────────── */}
        <section className="flex min-h-0 flex-col" aria-label="Results">
          <ResultPane
            run={w.run}
            query={w.text}
            role={deployment.role}
            onRewrite={w.rewrite}
            onNextPage={(after) => w.execute({ after: String(after) })}
          />
        </section>
      </div>

      {showHistory && (
        <aside className="hidden w-[264px] shrink-0 border-l border-line bg-surface-sunken lg:block" aria-label="Query history">
          <HistoryPanel
            entries={w.history.entries}
            onPick={w.pick}
            onTogglePin={w.history.togglePin}
            onClear={w.history.clear}
          />
        </aside>
      )}
    </div>
  );
}
