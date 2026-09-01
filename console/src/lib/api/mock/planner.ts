import { ApiError, type PqlResult, type QueryResult, type QueryTiming, type SqlResult } from "../types";
import { CARDINALITY, RECORD_COUNTS, SCHEMA, rng } from "./fixtures";

/**
 * A stand-in for `big-sql` / `big-plan` that is faithful in the one dimension
 * that matters to the UI: it refuses by name, at parse time, with a stable code
 * and the byte span of the construct it refused. Everything downstream — the
 * editor underline, the refusal panel, the one-click rewrite — reads that span.
 */

interface Refusal { code: string; message: string; span: [number, number]; status: number }

/** Ordered: the first construct in the text wins, the way a parser sees it. */
const SQL_RULES: Array<{ re: RegExp; code: string; message: string; status?: number }> = [
  { re: /\b(?:LEFT|RIGHT|FULL)(?:\s+OUTER)?\s+JOIN\b/i, code: "sql_no_outer_joins", message: "outer joins are not answered here" },
  { re: /\b(?:INNER\s+|CROSS\s+)?JOIN\b/i, code: "sql_no_joins", message: "no joins: a fact is one bit at (row, record)" },
  { re: /\bHAVING\b/i, code: "sql_unsupported", message: "HAVING has no aggregate to filter that the answer already holds" },
  { re: /\bOFFSET\b/i, code: "sql_unsupported", message: "OFFSET pages by a count that shifts under inserts" },
  { re: /\bUNION\b/i, code: "sql_union", message: "UNION compares rendered answers, not stored tuples" },
  { re: /\bWITH\s+[A-Za-z_]\w*\s+AS\s*\(/i, code: "sql_unsupported", message: "subquery / CTE: one plan per statement" },
  { re: /\b(?:IN|=|>|<)\s*\(\s*SELECT\b/i, code: "sql_unsupported", message: "subquery: one plan per statement" },
  { re: /\bOVER\s*\(/i, code: "sql_unsupported", message: "window functions need an ordered row stream" },
  { re: /\bIS\s+(?:NOT\s+)?NULL\b|\bNULL\b/i, code: "sql_no_nulls", message: "there are no nulls: a bit is set or it is not" },
  { re: /\b(?:UPDATE|TRUNCATE|MERGE|REPLACE)\b/i, code: "sql_read_only", message: "a fact is a bit: there is no row to change in place" },
  { re: /\bDELETE\s+FROM\b/i, code: "sql_read_only", message: "a record is bits across every field, not a row to delete" },
  { re: /\bINSERT\b(?![^]*?\()/i, code: "sql_insert_shape", message: "an INSERT names the columns it writes" },
  { re: /\b(?:CREATE|DROP|ALTER)\s+(?:DATABASE|SCHEMA)\b|\bUSE\s+[A-Za-z_]\w*/i, code: "sql_no_database", message: "there is no database above a table here" },
  { re: /\b(?:CREATE|DROP)\s+(?:MATERIALIZED\s+)?VIEW\b/i, code: "sql_no_views", message: "nothing here stores a statement" },
  { re: /\bcount\s*\(\s*DISTINCT\s+[A-Za-z_]\w*\s*,/i, code: "sql_unsupported", message: "count(DISTINCT a, b) is a composite key this index never stored" },
];

/**
 * Arithmetic in the select list, scoped to the select list so `count(*)` and
 * `SELECT *` -- which are not expressions -- do not trip it.
 */
function expressionRefusal(q: string): Refusal | null {
  const head = /\bSELECT\b([^]*?)\bFROM\b/i.exec(q);
  if (!head) return null;
  const list = head[1];
  const at = /[A-Za-z_)\d]\s*[-+/]\s*[A-Za-z_(\d]/.exec(list);
  if (!at) return null;
  const start = head.index + head[0].indexOf(list) + at.index;
  return {
    code: "sql_unsupported",
    message: "computed expression: there is no expression evaluator here",
    span: [start, start + at[0].length],
    status: 400,
  };
}

/**
 * The first construct in the text wins, the way a parser sees it. Ties break
 * toward the tighter span, so a rule that happens to match a wide region never
 * outranks the keyword actually at that position.
 */
function firstSqlRefusal(q: string): Refusal | null {
  const hits: Refusal[] = [];
  for (const rule of SQL_RULES) {
    const m = rule.re.exec(q);
    if (!m) continue;
    hits.push({ code: rule.code, message: rule.message, status: rule.status ?? 400,
      span: [m.index, m.index + m[0].length] });
  }
  const expr = expressionRefusal(q);
  if (expr) hits.push(expr);
  if (!hits.length) return null;
  hits.sort((a, b) => a.span[0] - b.span[0] || (a.span[1] - a.span[0]) - (b.span[1] - b.span[0]));
  return hits[0];
}

/** `SELECT <keyed column>` with no aggregate: refused, with a mechanical rewrite. */
function projectionRefusal(q: string): Refusal | null {
  const m = /SELECT\s+(?!DISTINCT\b)([A-Za-z_]\w*)\s+FROM\s+([A-Za-z_]\w*)/i.exec(q);
  if (!m) return null;
  const table = SCHEMA.tables.find((t) => t.name.toLowerCase() === m[2].toLowerCase());
  const field = table?.fields.find((f) => f.name.toLowerCase() === m[1].toLowerCase());
  if (!field || (field.kind !== "set" && field.kind !== "mutex")) return null;
  return {
    code: "sql_projection_unsupported",
    message: `\`${field.name}\` is a keyed column: the dictionary maps strings into rows, never back`,
    span: [m.index + m[0].toUpperCase().indexOf(m[1].toUpperCase()), m.index + m[0].toUpperCase().indexOf(m[1].toUpperCase()) + m[1].length],
    status: 400,
  };
}

function unknownTable(q: string): Refusal | null {
  const m = /\bFROM\s+([A-Za-z_]\w*)/i.exec(q);
  if (!m) return null;
  if (SCHEMA.tables.some((t) => t.name.toLowerCase() === m[1].toLowerCase())) return null;
  const at = m.index + m[0].length - m[1].length;
  return { code: "unknown_table", message: `no table \`${m[1]}\``, span: [at, at + m[1].length], status: 404 };
}

const PQL_CALLS = new Set([
  "Count", "Sum", "Min", "Max", "TopN", "GroupBy", "Distinct", "Rows", "Project",
  "Row", "All", "Not", "Intersect", "Union", "Difference",
]);

function pqlRefusal(q: string, after?: string): Refusal | null {
  const call = /([A-Za-z_]\w*)\s*\(/g;
  let m: RegExpExecArray | null;
  let depth = 0;
  let head: string | null = null;
  while ((m = call.exec(q))) {
    if (!head) head = m[1];
    if (!PQL_CALLS.has(m[1])) {
      return { code: "unknown_call", message: `no such call \`${m[1]}\``, span: [m.index, m.index + m[1].length], status: 400 };
    }
  }
  for (const ch of q) { if (ch === "(") depth++; }
  if (depth > 12) {
    return { code: "query_too_deep", message: "the query nests deeper than the limit of 12", span: [0, Math.min(q.length, 40)], status: 400 };
  }
  // Field references must exist.
  const fieldRef = /field\s*=\s*"([^"]*)"|([A-Za-z_]\w*)\s*=\s*"/g;
  const table = SCHEMA.tables[0];
  let f: RegExpExecArray | null;
  while ((f = fieldRef.exec(q))) {
    const name = f[1] ?? f[2];
    if (!name) continue;
    if (!table.fields.some((x) => x.name === name)) {
      const at = f.index + f[0].indexOf(name);
      return { code: "unknown_field", message: `no field \`${name}\` on \`${table.name}\``, span: [at, at + name.length], status: 400 };
    }
  }
  // `after=` only pages a record listing.
  if (after && head && head !== "Rows" && head !== "Project") {
    return {
      code: "not_pageable",
      message: `\`${head}\` returns one answer, not a page of records`,
      span: [0, head.length],
      status: 422,
    };
  }
  return null;
}

function timing(seed: number, shards = 4): QueryTiming {
  const r = rng(seed);
  const s = Array.from({ length: shards }, (_, i) => ({
    shard: `s${i}`,
    node: `big-0${(i % 4) + 1}`,
    us: Math.round(180 + r() * 2_600),
    rows_touched: Math.round(1_000 + r() * 480_000),
  }));
  const parse_us = Math.round(4 + r() * 12);
  const plan_us = Math.round(9 + r() * 30);
  const merge_us = Math.round(20 + r() * 90);
  return { parse_us, plan_us, merge_us, shards: s, total_us: parse_us + plan_us + Math.max(...s.map((x) => x.us)) + merge_us };
}

/* ── SQL execution ──────────────────────────────────────────────────────── */

function seedOf(q: string) {
  let h = 2166136261;
  for (let i = 0; i < q.length; i++) { h ^= q.charCodeAt(i); h = Math.imul(h, 16777619); }
  return h >>> 0;
}

/** `count(*)`, `sum(amount)` — the header echoes what was actually asked for. */
function label(a: { call: string; arg: string }): string {
  return `${a.call}(${a.arg || "*"})`;
}

function sqlResult(q: string): SqlResult {
  const r = rng(seedOf(q));
  const from = /\bFROM\s+([A-Za-z_]\w*)/i.exec(q)?.[1] ?? "events";
  const groupBy = /\bGROUP\s+BY\s+([A-Za-z_]\w*)/i.exec(q)?.[1];
  const limit = Number(/\bLIMIT\s+(\d+)/i.exec(q)?.[1] ?? 20);
  // Only the select list decides the columns. Scanning the whole statement
  // counted `ORDER BY count(*)` as a second aggregate and emitted it twice.
  const selectList = /\bSELECT\b([^]*?)\bFROM\b/i.exec(q)?.[1] ?? "";
  const aggMatch = [...selectList.matchAll(/\b(count|sum|min|max|avg|quantile)\s*\(\s*([^)]*)\)/gi)]
    .map((m) => ({ call: m[1].toLowerCase(), arg: m[2].trim() }));
  const total = RECORD_COUNTS[from] ?? 1_000_000;

  if (/^\s*CREATE\s+TABLE/i.test(q)) return { columns: ["result"], rows: [["ok"]] };

  if (groupBy) {
    const keys = KEYS[groupBy] ?? Array.from({ length: 24 }, (_, i) => `${groupBy}_${i}`);
    const cols = [groupBy, ...(aggMatch.length ? aggMatch.map(label) : ["count(*)"])];
    const rows = keys.slice(0, Math.min(limit, keys.length)).map((k) => {
      const c = Math.floor(r() * total * 0.02) + 1;
      return [k, ...cols.slice(1).map((cn) => (cn.startsWith("count(") ? c : Number((c * (0.8 + r() * 40)).toFixed(2))))];
    });
    rows.sort((a, b) => Number(b[1]) - Number(a[1]));
    return { columns: cols, rows };
  }
  if (aggMatch.length) {
    return {
      columns: aggMatch.map(label),
      rows: [aggMatch.map((a) => (a.call === "count"
        ? Math.floor(total * (0.02 + r() * 0.4))
        : Number((total * (0.0001 + r() * 0.01)).toFixed(2))))],
    };
  }
  return { columns: ["count(*)"], rows: [[Math.floor(total * (0.02 + r() * 0.4))]] };
}

const KEYS: Record<string, string[]> = {
  country: ["GB", "US", "DE", "FR", "NL", "ES", "IT", "SE", "PL", "IE", "BE", "DK", "NO", "FI", "PT", "AT", "CH", "CZ"],
  device: ["mobile", "desktop", "tablet", "tv", "console", "other"],
  browser: ["chrome", "safari", "firefox", "edge", "samsung", "opera", "brave"],
  os: ["ios", "android", "macos", "windows", "linux", "chromeos"],
  status: ["placed", "paid", "packed", "shipped", "delivered", "returned", "cancelled"],
  channel: ["web", "ios-app", "android-app", "partner", "phone"],
  campaign: Array.from({ length: 40 }, (_, i) => `cmp-2026-${String(i + 1).padStart(3, "0")}`),
  placement: Array.from({ length: 30 }, (_, i) => `slot-${String(i + 1).padStart(2, "0")}`),
};

/* ── PQL execution ──────────────────────────────────────────────────────── */

function pqlResult(q: string, table: string, limit: number, after?: string): PqlResult {
  const r = rng(seedOf(q));
  const head = /^\s*([A-Za-z_]\w*)\s*\(/.exec(q)?.[1] ?? "Count";
  const total = RECORD_COUNTS[table] ?? 1_000_000;
  const fieldArg = /field\s*=\s*"([^"]+)"/.exec(q)?.[1];
  const keyed = /([A-Za-z_]\w*)\s*=\s*"/.exec(q)?.[1];

  switch (head) {
    case "Count":
      return { shape: "count", value: Math.floor(total * (0.001 + r() * 0.3)) };
    case "Sum": case "Min": case "Max": {
      const f = SCHEMA.tables.find((t) => t.name === table)?.fields.find((x) => x.name === fieldArg);
      return { shape: "aggregate", call: head, field: fieldArg ?? "amount", scale: f?.scale,
        value: head === "Min" ? Number((r() * 90).toFixed(2)) : Math.floor(total * (0.4 + r() * 3)) };
    }
    case "TopN": {
      const field = fieldArg ?? keyed ?? "country";
      // `n=` is the call's own argument and outranks the request's `limit`.
      const n = Number(/\bn\s*=\s*(\d+)/.exec(q)?.[1]) || limit || 20;
      const keys = KEYS[field] ?? Array.from({ length: 50 }, (_, i) => `${field}-${i}`);
      const items = keys.slice(0, Math.min(n, keys.length))
        .map((k) => ({ key: k, count: Math.floor(total * (0.001 + r() * 0.09)) }))
        .sort((a, b) => b.count - a.count);
      return { shape: "topn", field, items };
    }
    case "GroupBy": {
      const field = fieldArg ?? keyed ?? "country";
      const agg = /aggregate\s*=\s*(\w+)\s*\(/.exec(q)?.[1];
      const keys = KEYS[field] ?? Array.from({ length: 30 }, (_, i) => `${field}-${i}`);
      return {
        shape: "groups", by: [field], aggregate: agg,
        groups: keys.slice(0, Math.min(limit || 24, keys.length)).map((k) => ({
          key: [k], value: agg ? Number((total * (0.00001 + r() * 0.004)).toFixed(2)) : Math.floor(total * (0.001 + r() * 0.06)),
        })).sort((a, b) => b.value - a.value),
      };
    }
    case "Distinct": {
      const field = fieldArg ?? keyed ?? "country";
      const keys = KEYS[field] ?? Array.from({ length: 60 }, (_, i) => `${field}-${i}`);
      return { shape: "distinct", field, values: keys.slice(0, limit || 60) };
    }
    default: {
      const start = after ? Number(after) : 0;
      const n = limit || 200;
      return {
        shape: "records", limit: n,
        ids: Array.from({ length: n }, (_, i) => start + i * 3 + Math.floor(r() * 3)),
        after: start + n * 3,
      };
    }
  }
}

/* ── entry points ───────────────────────────────────────────────────────── */

export interface Executed { result: QueryResult; timing: QueryTiming }

export function runSql(q: string): Executed {
  const refusal = firstSqlRefusal(q) ?? unknownTable(q) ?? projectionRefusal(q);
  if (refusal) throw new ApiError(refusal.status, { code: refusal.code, message: refusal.message }, refusal.span);
  if (!/^\s*(SELECT|CREATE)\b/i.test(q.trim())) {
    throw new ApiError(400, { code: "parse_error", message: "at byte 0: expected `SELECT`, found `" + q.trim().split(/\s/)[0] + "`" }, [0, Math.max(1, q.trim().split(/\s/)[0].length)]);
  }
  return { result: { kind: "sql", result: sqlResult(q) }, timing: timing(seedOf(q)) };
}

export function runPql(q: string, table: string, limit = 0, after?: string): Executed {
  if (!SCHEMA.tables.some((t) => t.name === table)) {
    throw new ApiError(404, { code: "unknown_table", message: `no table \`${table}\`` });
  }
  if (!/^\s*[A-Za-z_]\w*\s*\(/.test(q)) {
    throw new ApiError(400, { code: "parse_error", message: "at byte 0: expected a call, found `" + (q.trim()[0] ?? "end of input") + "`" }, [0, 1]);
  }
  const refusal = pqlRefusal(q, after);
  if (refusal) throw new ApiError(refusal.status, { code: refusal.code, message: refusal.message }, refusal.span);
  return { result: { kind: "pql", result: pqlResult(q, table, limit, after) }, timing: timing(seedOf(q)) };
}
