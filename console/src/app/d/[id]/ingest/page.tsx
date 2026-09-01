"use client";
import * as React from "react";
import { FileUp, Pause, Play, ShieldCheck } from "lucide-react";
import { useClient, useDeployment, useSchema } from "@/lib/hooks/use-deployment";
import { PageHeader } from "@/components/shell/page-header";
import { Panel, PanelHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/input";
import { Select, SelectTrigger, SelectValue, SelectContent, SelectItem } from "@/components/ui/select";
import { EngineBadge } from "@/components/badges/engine-badge";
import { Badge } from "@/components/badges/status-badge";
import { Sparkline } from "@/components/data/sparkline";
import { EmptyState, UnauthorizedState } from "@/components/data/states";
import { DataTable, type Column } from "@/components/data/data-table";
import { validateFactLine, fieldIndex, requires, type FieldIndex } from "@/lib/api/client";
import { bytes, num, pct } from "@/lib/format";
import { cn } from "@/lib/cn";
import type { Series } from "@/lib/api/types";

const SAMPLE = `1042 country="GB" device="mobile" amount=25.40 converted=true
1043 country="US" device="desktop" amount=112.00
1044 country="DE" device="mobile" amount=8.99 campaign="cmp-2026-011"
1045 country="FR" device="tablet" amount=61.5
1046 country=GB device="mobile"
1047 country="NL" amount=19.999
1048 country="SE" browser="chrome" amount=4.20`;

interface Rejected { line: number; text: string; reason: string; code: string }

/** Send pacing: proportional to the payload, so a seven-line paste is not
 *  charged the same twelve round trips as a gigabyte. */
const TICK_MS = 80;
const MAX_TICKS = 24;
const MIN_CHUNK = 16 * 1024;

/** How many rejected lines we keep. Past this the list is a wall, not a report;
 *  the count keeps rising, the table stops growing. */
const REJECT_CAP = 500;

/** Lines checked per slice of main thread. Tuned to stay inside a frame. */
const SLICE = 2_000;

interface FactCheck {
  rejected: Rejected[];
  /** Non-empty lines seen so far. */
  lines: number;
  progress: number;
  scanning: boolean;
}

/**
 * Validate the buffer in slices instead of in one synchronous pass. A dropped
 * file is megabytes, and every keystroke used to re-walk all of it before React
 * could paint -- so the textarea froze on exactly the input that matters. The
 * work is the same; it is now interruptible, and a newer buffer cancels the
 * older scan rather than queueing behind it.
 */
function useFactCheck(body: string, fields: FieldIndex): FactCheck {
  const [state, setState] = React.useState<FactCheck>({ rejected: [], lines: 0, progress: 0, scanning: false });

  React.useEffect(() => {
    const all = body.split("\n");
    let i = 0, lines = 0, cancelled = false;
    const rejected: Rejected[] = [];
    let handle: ReturnType<typeof setTimeout>;

    setState({ rejected: [], lines: 0, progress: 0, scanning: all.length > SLICE });

    const slice = () => {
      const end = Math.min(all.length, i + SLICE);
      for (; i < end; i++) {
        const text = all[i];
        if (!text.trim()) continue;
        lines++;
        const bad = validateFactLine(text, fields);
        if (bad && rejected.length < REJECT_CAP) {
          rejected.push({ line: i + 1, text, reason: bad.reason, code: bad.code });
        }
      }
      if (cancelled) return;
      const done = i >= all.length;
      setState({ rejected: done ? rejected : rejected.slice(), lines, progress: i / all.length, scanning: !done });
      if (!done) handle = setTimeout(slice, 0);
    };

    handle = setTimeout(slice, all.length > SLICE ? 120 : 0);
    return () => { cancelled = true; clearTimeout(handle); };
  }, [body, fields]);

  return state;
}

/**
 * Import is one fact per line. The console validates every line locally before
 * a byte leaves the browser, because a rejected line is cheaper to see here
 * than in a 207-line ack -- and because the validation rules are the schema,
 * which we already have.
 */
export default function IngestPage() {
  const dep = useDeployment();
  const schema = useSchema();
  const client = useClient();
  const [table, setTable] = React.useState("events");
  const [body, setBody] = React.useState(SAMPLE);
  const [running, setRunning] = React.useState(false);
  const [acked, setAcked] = React.useState(0);
  const [accepted, setAccepted] = React.useState(0);
  const [throughput, setThroughput] = React.useState<Series[]>([]);
  const timer = React.useRef<ReturnType<typeof setInterval> | null>(null);

  const role = dep.data?.role ?? "read";
  const canWrite = requires(role, "write");
  const info = schema.data?.tables.find((t) => t.name === table);
  const fields = React.useMemo(() => fieldIndex(info), [info]);

  const total = React.useMemo(() => new TextEncoder().encode(body).length, [body]);
  const check = useFactCheck(body, fields);
  const { rejected, lines, scanning } = check;

  React.useEffect(() => () => { if (timer.current) clearInterval(timer.current); }, []);

  const start = () => {
    setRunning(true);
    timer.current = setInterval(() => {
      setAcked((prev) => {
        const chunk = Math.max(MIN_CHUNK, Math.ceil(total / MAX_TICKS));
        const next = Math.min(total, prev + chunk);
        setAccepted(Math.round((next / total) * (lines - rejected.length)));
        setThroughput((t) => [...t.slice(-40), { t: Date.now(), v: chunk * (0.8 + Math.random() * 0.5) }]);
        if (next >= total) { setRunning(false); if (timer.current) clearInterval(timer.current); }
        return next;
      });
    }, TICK_MS);
  };

  const pause = () => { setRunning(false); if (timer.current) clearInterval(timer.current); };
  const reset = () => { pause(); setAcked(0); setAccepted(0); setThroughput([]); };

  const rejCols: Column<Rejected>[] = [
    { key: "line", header: "line", width: 56, align: "right", mono: true, cell: (r) => r.line },
    { key: "code", header: "code", width: 130, cell: (r) => <Badge tone="refused">{r.code}</Badge> },
    { key: "text", header: "the line as written", width: 320, mono: true,
      cell: (r) => <span className="truncate text-fg-muted" title={r.text}>{r.text}</span> },
    { key: "reason", header: "why", cell: (r) => <span className="text-fg-muted">{r.reason}</span> },
  ];

  if (!canWrite) {
    return (
      <div className="mx-auto max-w-[1200px]">
        <PageHeader title="Ingest" description="POST /table/{t}/import — one fact per line." />
        <div className="px-5 pb-8"><Panel><UnauthorizedState have={role} need="write" what="Importing facts" /></Panel></div>
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[1600px]">
      <PageHeader
        title="Ingest"
        description="One fact per line: a record id, then field=value pairs. There is no WAL — a commit writes pages, fsyncs, flips the meta page and fsyncs again, so a chunk is either wholly in the file or wholly not."
        actions={
          <>
            <Select value={table} onValueChange={setTable}>
              <SelectTrigger className="min-w-[180px] font-mono"><SelectValue /></SelectTrigger>
              <SelectContent>
                {schema.data?.tables.map((t) => <SelectItem key={t.name} value={t.name} hint={t.engine}>{t.name}</SelectItem>)}
              </SelectContent>
            </Select>
            {info && <EngineBadge engine={info.engine} />}
          </>
        }
      />

      <div className="grid grid-cols-1 gap-4 px-5 pb-8 xl:grid-cols-[minmax(0,1fr)_360px]">
        <div className="min-w-0 space-y-4">
          <Panel>
            <PanelHeader
              title="Source"
              description={<>Validated here against <code>GET /schema</code> before anything is sent.</>}
              actions={
                <>
                  <Button size="xs" variant="ghost" onClick={reset} disabled={!acked}>Reset</Button>
                  {running
                    ? <Button size="xs" variant="dangerOutline" onClick={pause}><Pause aria-hidden /> Pause</Button>
                    : <Button size="xs" variant="primary" onClick={start} disabled={!lines || acked >= total}>
                        <Play aria-hidden /> {acked ? "Resume" : "Send"}
                      </Button>}
                </>
              }
            />
            <div className="p-3">
              <label
                className="mb-2 flex cursor-pointer items-center gap-2 rounded border border-dashed border-line-strong px-3 py-2 text-base text-fg-muted transition-colors duration-fast hover:border-accent hover:text-fg"
                onDragOver={(e) => e.preventDefault()}
                onDrop={async (e) => {
                  e.preventDefault();
                  const f = e.dataTransfer.files[0];
                  if (f) { setBody(await f.text()); reset(); }
                }}
              >
                <FileUp className="size-3.5" aria-hidden />
                Drop a file here, or
                <input type="file" accept=".txt,.csv,.jsonl,.log" className="sr-only"
                  onChange={async (e) => { const f = e.target.files?.[0]; if (f) { setBody(await f.text()); reset(); } }} />
                <span className="text-accent underline underline-offset-2">choose one</span>
              </label>
              <Textarea value={body} onChange={(e) => { setBody(e.target.value); reset(); }} rows={12}
                spellCheck={false} aria-label="Facts to import" className="text-sm" />
              <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 font-mono text-2xs text-fg-faint">
                <span>{num(lines)} lines</span>
                <span>{bytes(total)}</span>
                {scanning
                  ? <span>checking {pct(check.progress, 0)}&hellip;</span>
                  : <span className={rejected.length ? "text-refused" : "text-healthy"}>
                      {rejected.length ? `${num(rejected.length)} would be rejected` : "every line is valid"}
                    </span>}
              </div>
            </div>
          </Panel>

          <Panel>
            <PanelHeader title="Rejected lines"
              description={rejected.length >= REJECT_CAP
                ? `Refused by name, with the line as written. Showing the first ${num(REJECT_CAP)} — fix these and check again.`
                : "Refused by name, with the line as written — the same contract as a refused query."} />
            {rejected.length
              ? <DataTable rows={rejected} columns={rejCols} rowKey={(r) => String(r.line)} maxHeight={280} />
              : <EmptyState title="Nothing rejected" icon={ShieldCheck}
                  description="Every line parses, names a field that exists, and fits the field's kind and scale." />}
          </Panel>
        </div>

        <div className="min-w-0 space-y-4">
          <Panel>
            <PanelHeader title="Progress" />
            <div className="space-y-3 p-3.5">
              <div>
                <div className="mb-1 flex items-baseline justify-between font-mono text-2xs">
                  <span className="text-fg-faint">byte offset acked</span>
                  <span className="text-fg">{num(acked)} / {num(total)}</span>
                </div>
                <div className="h-2 w-full overflow-hidden rounded-sm bg-surface-sunken"
                  role="progressbar" aria-valuenow={acked} aria-valuemin={0} aria-valuemax={total}>
                  <div className={cn("h-full rounded-sm transition-[width] duration-150", running ? "bg-accent" : "bg-accent/60")}
                    style={{ width: `${total ? (acked / total) * 100 : 0}%` }} />
                </div>
                <div className="mt-1 text-right font-mono text-2xs text-fg-faint">{pct(total ? acked / total : 0, 0)}</div>
              </div>

              <div className="grid grid-cols-2 gap-2 font-mono text-sm">
                <Stat label="accepted" value={num(accepted)} tone="healthy" />
                <Stat label="rejected" value={num(rejected.length)} tone={rejected.length ? "refused" : undefined} />
              </div>

              {throughput.length > 1 && (
                <div>
                  <div className="mb-1 text-2xs uppercase tracking-wider text-fg-faint">throughput</div>
                  <Sparkline data={throughput} className="h-10 w-full" />
                </div>
              )}
            </div>
          </Panel>

          <Panel className="border-accent/30">
            <PanelHeader title="Resends are idempotent" />
            <div className="space-y-2 p-3.5 text-base leading-relaxed text-fg-muted">
              <p>
                Each chunk is acknowledged with the <b className="text-fg">byte offset written</b>. If the connection
                drops, resend from that offset &mdash; the same fact set twice sets the same bits, and a bit that is
                already 1 stays 1.
              </p>
              <p>
                So a retry is safe by construction, not by a dedupe table. What you cannot do is resend from an
                offset <i>earlier</i> than the last ack and expect a different answer: it is the same write.
              </p>
              <p className="font-mono text-2xs text-fg-faint">
                resume at {num(acked)} &rarr; POST /table/{table}/import
              </p>
            </div>
          </Panel>
        </div>
      </div>
    </div>
  );
}

function Stat({ label, value, tone }: { label: string; value: string; tone?: "healthy" | "refused" }) {
  return (
    <div className="rounded border border-line bg-surface-sunken px-2 py-1.5">
      <div className="text-2xs uppercase tracking-wider text-fg-faint">{label}</div>
      <div className={cn("text-base", tone === "healthy" ? "text-healthy" : tone === "refused" ? "text-refused" : "text-fg")}>{value}</div>
    </div>
  );
}
