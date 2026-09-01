"use client";
import * as React from "react";

export interface HistoryEntry {
  id: string;
  surface: "sql" | "pql";
  table?: string;
  text: string;
  at: number;
  /** null when it succeeded; the stable code when it was refused or failed. */
  code: string | null;
  duration_us?: number;
  pinned?: boolean;
}

const KEY = "bigdb-query-history";

/** Query history lives in the browser: it is the user's, not the deployment's. */
export function useQueryHistory(deploymentId: string) {
  const [entries, setEntries] = React.useState<HistoryEntry[]>([]);

  React.useEffect(() => {
    try {
      const raw = localStorage.getItem(`${KEY}:${deploymentId}`);
      if (raw) setEntries(JSON.parse(raw));
    } catch {}
  }, [deploymentId]);

  const add = React.useCallback((e: Omit<HistoryEntry, "id" | "at">) => {
    setEntries((prev) => {
      const next = [{ ...e, id: `${Date.now()}-${Math.random().toString(36).slice(2, 7)}`, at: Date.now() }, ...prev].slice(0, 120);
      try { localStorage.setItem(`${KEY}:${deploymentId}`, JSON.stringify(next)); } catch {}
      return next;
    });
  }, [deploymentId]);

  const togglePin = React.useCallback((id: string) => {
    setEntries((prev) => {
      const next = prev.map((e) => (e.id === id ? { ...e, pinned: !e.pinned } : e));
      try { localStorage.setItem(`${KEY}:${deploymentId}`, JSON.stringify(next)); } catch {}
      return next;
    });
  }, [deploymentId]);

  const clear = React.useCallback(() => {
    setEntries((prev) => {
      const next = prev.filter((e) => e.pinned);
      try { localStorage.setItem(`${KEY}:${deploymentId}`, JSON.stringify(next)); } catch {}
      return next;
    });
  }, [deploymentId]);

  // Stable identity: `add`, `togglePin` and `clear` are the dependencies callers
  // reach for, and a fresh object every render would restart their effects.
  return React.useMemo(() => ({ entries, add, togglePin, clear }), [entries, add, togglePin, clear]);
}

/** A permalink carries the whole query, so a link in a ticket still runs. */
export function encodeQuery(surface: "sql" | "pql", table: string, text: string): string {
  const payload = JSON.stringify({ s: surface, t: table, q: text });
  return typeof window === "undefined" ? "" : btoa(unescape(encodeURIComponent(payload)));
}

export function decodeQuery(hash: string): { surface: "sql" | "pql"; table: string; text: string } | null {
  try {
    const { s, t, q } = JSON.parse(decodeURIComponent(escape(atob(hash))));
    if (typeof q !== "string") return null;
    return { surface: s === "pql" ? "pql" : "sql", table: t ?? "", text: q };
  } catch { return null; }
}
