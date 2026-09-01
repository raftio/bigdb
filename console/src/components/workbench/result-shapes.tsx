"use client";
import * as React from "react";
import { Copy, Check } from "lucide-react";
import { DataTable, type Column } from "@/components/data/data-table";
import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/data/states";
import { Tooltip } from "@/components/ui/tooltip";
import { num, scaled } from "@/lib/format";
import type { PqlResult, SqlResult } from "@/lib/api/types";

/**
 * A PQL answer is not JSON to be pretty-printed. It is a count, an aggregate, a
 * ranking, a set of groups, a set of keys, or a page of record ids -- six
 * shapes, each with a reading that suits it. A count is one enormous number; a
 * ranking is bars with the counts still exact beside them; a page of ids is a
 * dense grid, because that is what a thousand integers want to be.
 */

export function CountResult({ value, label = "count" }: { value: number; label?: string }) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 p-8">
      <div className="font-mono text-3xl tabular text-fg" aria-live="polite">{num(value)}</div>
      <div className="text-sm uppercase tracking-widest text-fg-faint">{label}</div>
      <p className="mt-2 max-w-sm text-center text-sm leading-relaxed text-fg-faint">
        One popcount over the intersected bitmap &mdash; not a scan, and not an estimate.
      </p>
    </div>
  );
}

export function AggregateResult({ call, field, value, scale }: {
  call: string; field: string; value: number; scale?: number;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-2 p-8">
      <div className="font-mono text-3xl tabular text-fg" aria-live="polite">{scaled(value, scale)}</div>
      <div className="text-sm uppercase tracking-widest text-fg-faint">
        {call.toLowerCase()}(<span className="font-mono normal-case tracking-normal text-fg-muted">{field}</span>)
      </div>
      {scale ? (
        <p className="mt-2 max-w-sm text-center text-sm leading-relaxed text-fg-faint">
          <code>{field}</code> is a decimal with scale {scale}: the engine stores {num(value * 10 ** scale)} and the
          scale is what makes it a quantity.
        </p>
      ) : null}
    </div>
  );
}

/** A ranking reads as bars, but the counts stay exact next to them. */
export function TopNResult({ field, items }: { field: string; items: Array<{ key: string; count: number }> }) {
  const max = Math.max(...items.map((i) => i.count), 1);
  if (!items.length) {
    return <EmptyState title="No rows in this ranking"
      description="Every row of this field is empty for the records the filter selected." />;
  }
  return (
    <div className="h-full overflow-auto p-3">
      <div className="mb-2 flex items-baseline justify-between">
        <span className="text-2xs uppercase tracking-wider text-fg-faint">top {items.length} by count &middot; {field}</span>
        <span className="font-mono text-2xs text-fg-faint">{num(items.reduce((a, b) => a + b.count, 0))} total</span>
      </div>
      <ol className="space-y-px">
        {items.map((it, i) => (
          <li key={it.key} className="grid grid-cols-[26px_minmax(90px,180px)_1fr_92px] items-center gap-2 rounded-sm px-1 py-1 hover:bg-surface-raised">
            <span className="text-right font-mono text-2xs text-fg-faint">{i + 1}</span>
            <span className="truncate font-mono text-base text-fg" title={it.key}>{it.key}</span>
            <span className="h-2 overflow-hidden rounded-sm bg-surface-sunken">
              <span className="block h-full rounded-sm bg-accent/70" style={{ width: `${(it.count / max) * 100}%` }} />
            </span>
            <span className="text-right font-mono text-base tabular text-fg-muted">{num(it.count)}</span>
          </li>
        ))}
      </ol>
    </div>
  );
}

export function GroupsResult({ by, aggregate, groups }: {
  by: string[]; aggregate?: string; groups: Array<{ key: string[]; value: number }>;
}) {
  const max = Math.max(...groups.map((x) => x.value), 1);
  const columns: Column<{ key: string[]; value: number }>[] = [
    ...by.map((b, i) => ({
      key: b, header: b, width: 200, mono: true,
      cell: (g: { key: string[] }) => <span className="text-fg">{g.key[i]}</span>,
      sort: (a: { key: string[] }, c: { key: string[] }) => a.key[i].localeCompare(c.key[i]),
    })),
    {
      key: "value", header: aggregate ? `${aggregate.toLowerCase()}(...)` : "count(*)",
      align: "right" as const, mono: true, width: 140,
      cell: (g: { value: number }) => <span className="text-fg">{num(g.value)}</span>,
      sort: (a: { value: number }, c: { value: number }) => a.value - c.value,
    },
    {
      key: "bar", header: "", width: 160,
      cell: (g: { value: number }) => (
        <span className="block h-1.5 overflow-hidden rounded-sm bg-surface-sunken">
          <span className="block h-full bg-accent/60" style={{ width: `${(g.value / max) * 100}%` }} />
        </span>
      ),
    },
  ];
  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="shrink-0 border-b border-line px-3 py-1.5 text-2xs uppercase tracking-wider text-fg-faint">
        {num(groups.length)} groups by {by.join(", ")}{aggregate ? ` · aggregate ${aggregate}` : ""}
      </div>
      <DataTable rows={groups} columns={columns} rowKey={(g) => g.key.join(" ")} className="min-h-0 flex-1" maxHeight={10_000} />
    </div>
  );
}

export function DistinctResult({ field, values }: { field: string; values: string[] }) {
  return (
    <div className="h-full overflow-auto p-3">
      <div className="mb-2 text-2xs uppercase tracking-wider text-fg-faint">
        {num(values.length)} distinct keys &middot; {field}
      </div>
      <div className="flex flex-wrap gap-1">
        {values.map((v) => (
          <span key={v} className="rounded-sm border border-line bg-surface-raised px-1.5 py-0.5 font-mono text-sm text-fg">{v}</span>
        ))}
      </div>
    </div>
  );
}

/** A page of record ids: dense, monospaced, and honest about being a page. */
export function RecordsResult({ ids, after, limit, onNextPage }: {
  ids: number[]; after?: number | null; limit: number; onNextPage?: (after: number) => void;
}) {
  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center gap-3 border-b border-line px-3 py-1.5">
        <span className="text-2xs uppercase tracking-wider text-fg-faint">{num(ids.length)} record ids &middot; limit {limit}</span>
        <span className="ml-auto font-mono text-2xs text-fg-faint">
          cursor <span className="text-fg-muted">after={after ?? "--"}</span>
        </span>
        {after != null && onNextPage && (
          <Button size="xs" variant="outline" onClick={() => onNextPage(after)}>Next page</Button>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-2">
        <div className="grid grid-cols-[repeat(auto-fill,minmax(92px,1fr))] gap-x-2 gap-y-px">
          {ids.map((id) => (
            <span key={id} className="rounded-sm px-1 font-mono text-sm tabular text-fg-muted hover:bg-surface-raised hover:text-fg">{id}</span>
          ))}
        </div>
      </div>
      <p className="shrink-0 border-t border-line px-3 py-1.5 text-2xs text-fg-faint">
        Paged by cursor, not by offset &mdash; a cursor does not shift when records are inserted under it.
      </p>
    </div>
  );
}

/** POST /sql returns {columns, rows}. Rendered as a virtualized table. */
export function SqlResultTable({ result }: { result: SqlResult }) {
  const columns: Column<Array<string | number | null>>[] = result.columns.map((c, i) => ({
    key: `${c}-${i}`,
    header: c,
    mono: true,
    align: typeof result.rows[0]?.[i] === "number" ? ("right" as const) : ("left" as const),
    width: i === 0 ? 220 : undefined,
    cell: (row: Array<string | number | null>) => {
      const v = row[i];
      return v === null
        ? <span className="text-fg-faint">&middot;</span>
        : <span className={typeof v === "number" ? "text-fg" : "text-fg-muted"}>{typeof v === "number" ? num(v) : String(v)}</span>;
    },
    sort: (a: Array<string | number | null>, b: Array<string | number | null>) => {
      const [x, y] = [a[i], b[i]];
      if (typeof x === "number" && typeof y === "number") return x - y;
      return String(x).localeCompare(String(y));
    },
  }));

  if (!result.rows.length) {
    return <EmptyState title="Zero rows" description="The statement was answered. Nothing matched." />;
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="shrink-0 border-b border-line px-3 py-1.5 text-2xs uppercase tracking-wider text-fg-faint">
        {num(result.rows.length)} rows &times; {result.columns.length} columns
      </div>
      <DataTable rows={result.rows} columns={columns} rowKey={(_, i) => String(i)}
        className="min-h-0 flex-1" maxHeight={10_000} />
    </div>
  );
}

export function CopyJson({ value }: { value: unknown }) {
  const [copied, setCopied] = React.useState(false);
  return (
    <Tooltip content="Copy the raw response body">
      <Button size="iconSm" variant="ghost" aria-label="Copy JSON"
        onClick={() => {
          navigator.clipboard?.writeText(JSON.stringify(value, null, 2));
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        }}>
        {copied ? <Check className="text-healthy" aria-hidden /> : <Copy aria-hidden />}
      </Button>
    </Tooltip>
  );
}

export function PqlResultView({ result, onNextPage }: { result: PqlResult; onNextPage?: (after: number) => void }) {
  switch (result.shape) {
    case "count": return <CountResult value={result.value} />;
    case "aggregate": return <AggregateResult {...result} />;
    case "topn": return <TopNResult field={result.field} items={result.items} />;
    case "groups": return <GroupsResult by={result.by} aggregate={result.aggregate} groups={result.groups} />;
    case "distinct": return <DistinctResult field={result.field} values={result.values} />;
    case "records": return <RecordsResult ids={result.ids} after={result.after} limit={result.limit} onNextPage={onNextPage} />;
  }
}
