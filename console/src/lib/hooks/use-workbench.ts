"use client";
import * as React from "react";
import { ApiError, type Deployment, type QueryResult, type QueryTiming } from "@/lib/api/types";
import { BigClient } from "@/lib/api/client";
import { isRefusal } from "@/lib/refusals";
import { useQueryHistory, encodeQuery, decodeQuery, type HistoryEntry } from "./use-history";

export type Surface = "sql" | "pql";

export interface RunState {
  status: "idle" | "running" | "ok" | "refused" | "failed";
  result?: QueryResult;
  timing?: QueryTiming;
  error?: ApiError;
}

const SEED: Record<Surface, string> = {
  sql: `SELECT country, count(*)\nFROM events\nWHERE device = 'mobile' AND amount > 25.00\nGROUP BY country\nORDER BY count(*) DESC\nLIMIT 20`,
  pql: `Count(\n  Intersect(\n    Row(device="mobile"),\n    Union(Row(country="GB"), Row(country="IE"))\n  )\n)`,
};

/**
 * One run button, two surfaces, one result pane.
 *
 * A refusal is a distinct terminal state -- not an error. It keeps the previous
 * result on screen (you have not lost your place), it carries the byte span so
 * the editor can underline the construct, and it is recorded in history so the
 * lesson is retrievable.
 */
export function useWorkbench(deployment: Deployment) {
  const client = React.useMemo(() => new BigClient(deployment.id, deployment.role), [deployment]);
  const history = useQueryHistory(deployment.id);
  const addHistory = history.add;

  const [surface, setSurface] = React.useState<Surface>("sql");
  const [table, setTable] = React.useState("events");
  const [sql, setSql] = React.useState(SEED.sql);
  const [pql, setPql] = React.useState(SEED.pql);
  const [limit, setLimit] = React.useState(200);
  const [run, setRun] = React.useState<RunState>({ status: "idle" });
  const abort = React.useRef<AbortController | null>(null);

  const text = surface === "sql" ? sql : pql;
  const setText = surface === "sql" ? setSql : setPql;

  /* A permalink carries the whole query, so a link in a ticket still runs. */
  React.useEffect(() => {
    const hash = window.location.hash.replace(/^#q=/, "");
    if (!hash) return;
    const decoded = decodeQuery(hash);
    if (!decoded) return;
    setSurface(decoded.surface);
    if (decoded.table) setTable(decoded.table);
    if (decoded.surface === "sql") setSql(decoded.text); else setPql(decoded.text);
  }, []);

  const permalink = React.useCallback(() => {
    const url = `${window.location.origin}${window.location.pathname}#q=${encodeQuery(surface, table, text)}`;
    window.history.replaceState(null, "", url);
    return url;
  }, [surface, table, text]);

  const execute = React.useCallback(async (opts: { after?: string } = {}) => {
    abort.current?.abort();
    const controller = new AbortController();
    abort.current = controller;
    setRun((prev) => ({ ...prev, status: "running" }));

    const started = performance.now();
    try {
      const out = surface === "sql"
        ? await client.sql(sql, { signal: controller.signal })
        : await client.query(table, pql, { limit, after: opts.after, signal: controller.signal });
      setRun({ status: "ok", result: out.result, timing: out.timing });
      addHistory({ surface, table: surface === "pql" ? table : undefined, text, code: null, duration_us: out.timing.total_us });
    } catch (e) {
      const err = e instanceof ApiError ? e : new ApiError(500, { code: "client_error", message: String(e) });
      if (err.code === "query_cancelled") {
        setRun((prev) => ({ ...prev, status: "idle" }));
        return;
      }
      setRun((prev) => ({
        // The previous answer stays on screen: a refusal did not invalidate it.
        result: prev.result, timing: prev.timing,
        status: isRefusal(err.code) ? "refused" : "failed",
        error: err,
      }));
      addHistory({ surface, table: surface === "pql" ? table : undefined, text, code: err.code,
        duration_us: Math.round((performance.now() - started) * 1000) });
    } finally {
      abort.current = null;
    }
  }, [client, surface, sql, pql, table, limit, text, addHistory]);

  /** Wired to the query deadline: the server sees the connection close. */
  const cancel = React.useCallback(() => abort.current?.abort(), []);

  const pick = React.useCallback((e: HistoryEntry) => {
    setSurface(e.surface);
    if (e.table) setTable(e.table);
    if (e.surface === "sql") setSql(e.text); else setPql(e.text);
    setRun({ status: "idle" });
  }, []);

  const rewrite = React.useCallback((next: string) => {
    setText(next);
    setRun({ status: "idle" });
  }, [setText]);

  return {
    client, surface, setSurface, table, setTable, text, setText, limit, setLimit,
    run, execute, cancel, permalink, history, pick, rewrite,
    refusedSpan: run.status === "refused" || run.status === "failed" ? run.error?.span ?? null : null,
  };
}
