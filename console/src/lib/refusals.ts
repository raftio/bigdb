/**
 * The refusal catalogue.
 *
 * A refusal is not an error. It is the planner telling you, by name and at parse
 * time, that the construct you wrote does not exist here — and what does. Every
 * entry carries a stable code, one sentence of reason, and the alternative that
 * exists instead. Where the alternative is mechanical, `rewrite` produces it.
 *
 * Codes are the ones `big-sql` and `big-plan` actually emit.
 */

export type RefusalKind = "refusal" | "error";

export interface Refusal {
  code: string;
  /** The construct, named. This is the headline — never "Error". */
  construct: string;
  /** One sentence: why this engine has nothing to give here. */
  reason: string;
  /** What exists instead, in the user's own terms. */
  instead: string;
  /** A mechanical rewrite, when one exists. Returns null when judgement is needed. */
  rewrite?: (query: string) => string | null;
  /** Docs anchor. */
  topic: string;
}

const stripClause = (q: string, re: RegExp) => q.replace(re, "").replace(/[ \t]+\n/g, "\n").trimEnd();

export const REFUSALS: Record<string, Refusal> = {
  sql_no_joins: {
    code: "sql_no_joins",
    construct: "JOIN",
    reason:
      "There are no joins here: a fact is one bit at (row, record), and two tables share no record space to pair across.",
    instead:
      "Model the joined column as a field on the one table the records belong to, then filter and group on it in a single statement.",
    topic: "no-joins",
    rewrite: (q) => {
      const from = /\bFROM\s+([A-Za-z_]\w*)(?:\s+(?:AS\s+)?([A-Za-z_]\w*))?/i.exec(q);
      if (!from) return null;
      const [, tableName, alias] = from;
      let out = stripClause(q, /\s+(?:INNER\s+|LEFT\s+|RIGHT\s+|FULL\s+|CROSS\s+)?(?:OUTER\s+)?JOIN\s+[\s\S]*?(?=\s+WHERE\b|\s+GROUP\b|\s+ORDER\b|\s+LIMIT\b|$)/i);
      if (alias && !/^(where|group|order|limit)$/i.test(alias)) {
        // Aliases only existed to disambiguate the join; without it they are noise.
        out = out.replace(new RegExp(`\\b${alias}\\.`, "g"), "")
                 .replace(new RegExp(`(FROM\\s+${tableName})\\s+(?:AS\\s+)?${alias}\\b`, "i"), "$1");
      }
      return out;
    },
  },
  sql_no_outer_joins: {
    code: "sql_no_outer_joins",
    construct: "OUTER JOIN",
    reason:
      "An outer join must produce a row for a record with no partner, and a bitmap has no row to null out half of.",
    instead: "Model the column on one table; a record that never carried the value simply has the bit unset.",
    topic: "no-joins",
  },
  sql_unsupported_order: {
    code: "sql_unsupported_order",
    construct: "ORDER BY",
    reason:
      "A record listing has no stored values to order by, and a grouped answer holds exactly one number per group.",
    instead:
      "Order a grouped answer by the grouped column or by the one aggregate it selects — `ORDER BY count(*) DESC` is the ranking TopN already carries.",
    topic: "ordering",
  },
  sql_projection_unsupported: {
    code: "sql_projection_unsupported",
    construct: "SELECT <keyed column>",
    reason:
      "A keyed column has no read back from a record to its string — the dictionary maps one way, into rows.",
    instead: "Count it, aggregate it, or GROUP BY it. `SELECT DISTINCT country` is `GROUP BY country`.",
    topic: "projection",
    rewrite: (q) => {
      const m = q.match(/SELECT\s+(?:DISTINCT\s+)?([A-Za-z_][\w]*)\s+FROM\s+([A-Za-z_][\w]*)/i);
      if (!m) return null;
      return `SELECT ${m[1]}, count(*)\nFROM ${m[2]}\nGROUP BY ${m[1]}\nORDER BY count(*) DESC\nLIMIT 100`;
    },
  },
  sql_no_nulls: {
    code: "sql_no_nulls",
    construct: "NULL",
    reason:
      "There are no nulls here: a record either carries a value or the bit is not set, and neither is a null that comparisons propagate.",
    instead: "Ask for the records that do carry a value, and take the complement with `NOT` for the ones that do not.",
    topic: "no-nulls",
  },
  sql_read_only: {
    code: "sql_read_only",
    construct: "UPDATE / DELETE / TRUNCATE",
    reason:
      "A fact is one bit at (row, record), so there is no row to change in place and none to delete: changing one means writing the new fact and clearing the old.",
    instead:
      "Write with `INSERT INTO t (id, …) VALUES (…)` or `POST /table/{t}/import`. Remove records with `POST /table/{t}/delete`, which takes their ids.",
    topic: "writes",
  },
  sql_insert_shape: {
    code: "sql_insert_shape",
    construct: "INSERT without a column list",
    reason:
      "A column list is what says which field each value is for, and this translation holds no schema — the values would be positional against a field order the statement does not carry.",
    instead:
      "Name the columns: `INSERT INTO t (country, amount) VALUES ('GB', 100)`. The record id is optional; write `_record_id` to choose one, or leave it out and the server allocates.",
    topic: "writes",
  },
  sql_id_column: {
    code: "sql_id_column",
    construct: "a field called _record_id",
    reason:
      "`_record_id` is what a record is called here, not a field it can hold — an INSERT reads that column as the record to write about, so a field of that name could never be filled.",
    instead:
      "Name the field something else; `id` is free, which is why the reserved one is underscored. The record id is already there: `SELECT *` answers with it, and leaving it out of an INSERT has the server allocate one.",
    topic: "ddl",
  },
  sql_insert_too_large: {
    code: "sql_insert_too_large",
    construct: "INSERT of more than 10,000 rows",
    reason:
      "The whole statement is lexed and parsed into literals before the first fact is written, so the batch is resident twice over.",
    instead: "`POST /table/{t}/import`, one fact per line, with no statement to hold.",
    topic: "writes",
  },
  sql_no_database: {
    code: "sql_no_database",
    construct: "CREATE / DROP DATABASE, USE",
    reason:
      "There is no database above a table here: a node holds one catalog, and a table's name is what resolves a fact all the way to a bitmap.",
    instead: "`CREATE TABLE`, `DROP TABLE` and `SHOW TABLES` are the whole of the namespace.",
    topic: "ddl",
  },
  sql_no_views: {
    code: "sql_no_views",
    construct: "CREATE VIEW / MATERIALIZED VIEW",
    reason:
      "Nothing here stores a statement — the catalog holds tables, fields and keys, and a view is a statement kept under a name.",
    instead:
      "Write the SELECT where it is used, or create a table and write the answer into it — the materialised half, said out loud.",
    topic: "ddl",
  },
  sql_ambiguous_column: {
    code: "sql_ambiguous_column",
    construct: "unqualified column",
    reason: "The name resolves against exactly one table, and the statement names more than one.",
    instead: "A statement reads one table. Name it once in FROM and leave the columns bare.",
    topic: "single-table",
  },
  sql_too_many_aggregates: {
    code: "sql_too_many_aggregates",
    construct: "aggregate list",
    reason: "Each aggregate is a plan of its own, fanned out and merged separately — the count is what the statement costs in round trips.",
    instead: "Ask for at most 16, counting `avg` as two (a sum and a count).",
    topic: "aggregates",
  },
  sql_union: {
    code: "sql_union",
    construct: "UNION",
    reason: "A row here is a rendered answer rather than a stored tuple; there is nothing to deduplicate two of them by.",
    instead: "Combine sets of records inside `WHERE` with `AND`, `OR` and `NOT` — that union happens in the bitmap.",
    topic: "set-ops",
  },
  sql_unsupported: {
    code: "sql_unsupported",
    construct: "unsupported construct",
    reason: "The statement asks for something this planner has no plan for.",
    instead: "A statement answers one question: an aggregate, or a grouped column and one aggregate of it.",
    topic: "single-table",
  },
  /* `sql_unsupported` is emitted for several constructs; these refine it by the
     text the parser located, so the panel can still name the construct. */
  sql_unsupported__having: {
    code: "sql_unsupported",
    construct: "HAVING",
    reason:
      "A grouped answer holds one number per group, so a HAVING on a second one would filter on a number that is not there.",
    instead: "Filter the input in `WHERE`, or take the ranking `ORDER BY count(*) DESC LIMIT n` gives you and cut client-side.",
    topic: "grouping",
    rewrite: (q) => stripClause(q, /\s+HAVING\s+[\s\S]*?(?=\s+ORDER\b|\s+LIMIT\b|$)/i),
  },
  sql_unsupported__offset: {
    code: "sql_unsupported",
    construct: "OFFSET",
    reason:
      "A skip count shifts when records are inserted under it; the engine will not hand you a page that silently moved.",
    instead: "Page with the `after` cursor on `POST /table/{t}/query?after=&limit=` — a cursor does not shift.",
    topic: "paging",
    rewrite: (q) => stripClause(q, /\s+OFFSET\s+\d+/i),
  },
  sql_unsupported__subquery: {
    code: "sql_unsupported",
    construct: "subquery / CTE",
    reason: "There is one plan per statement, and a nested SELECT would be a second one whose result has no place to live.",
    instead: "Combine sets of records inside `WHERE` with `AND`, `OR` and `NOT`.",
    topic: "single-table",
  },
  sql_unsupported__window: {
    code: "sql_unsupported",
    construct: "window function",
    reason: "A window needs an ordered row stream, and an answer here is a count per key, not a stream.",
    instead: "`TopN` gives the ranking; the rest is arithmetic on the client.",
    topic: "grouping",
  },
  sql_unsupported__multi_distinct: {
    code: "sql_unsupported",
    construct: "count(DISTINCT a, b)",
    reason: "Two distinct columns is a grouping over a composite key this index never stored.",
    instead: "`DISTINCT` takes one keyed column — `SELECT DISTINCT c` (which is `GROUP BY c`) or `count(DISTINCT c)`.",
    topic: "distinct",
  },
  sql_unsupported__expression: {
    code: "sql_unsupported",
    construct: "computed expression",
    reason: "There is no expression evaluator here: the select list takes a column or an aggregate of one.",
    instead: "Select the parts and compute the arithmetic on the client.",
    topic: "projection",
  },
  sql_no_time_window: {
    code: "sql_no_time_window",
    construct: "time window",
    reason: "A field with no views by time has no window to answer — only every record it ever held.",
    instead: "Create the field with `kind=time_quantum&granularity=YMDH`, then bound it with `BETWEEN`.",
    topic: "time",
  },
  sql_quantile_level: {
    code: "sql_quantile_level",
    construct: "quantile(level)",
    reason: "A finer level names a place in the distribution no number of records here could resolve.",
    instead: "Use a level between 0 and 1 with at most three digits, e.g. `quantile(0.95)(amount)`.",
    topic: "aggregates",
  },
  sql_unknown_format: {
    code: "sql_unknown_format",
    construct: "FORMAT",
    reason: "The name does not spell any bytes this surface produces.",
    instead: "JSON, JSONCompact, TSV, TabSeparated, TSVWithNames, CSV, CSVWithNames.",
    topic: "formats",
  },

  unquoted_value: {
    code: "unquoted_value",
    construct: "unquoted value on a keyed field",
    reason: "A keyed value is interned into the row-key dictionary verbatim, and the line format needs quotes to know where it ends.",
    instead: 'Write it as `field="value"`. Only numeric fields take a bare token.',
    topic: "ingest",
  },

  /* ── PQL-side refusals ───────────────────────────────────────────────── */
  unknown_call: {
    code: "unknown_call",
    construct: "unknown call",
    reason: "The planner has no call by that name.",
    instead:
      "Rows: `Row`, `All`, `Not`, `Intersect`, `Union`, `Difference`. Answers: `Count`, `TopN`, `GroupBy`, `Distinct`, `Rows`, `Sum`, `Min`, `Max`.",
    topic: "pql",
  },
  bad_arity: {
    code: "bad_arity",
    construct: "wrong argument count",
    reason: "The call exists but was handed a different number of arguments than it takes.",
    instead: "Check the signature in the completion popup — every call lists its arity inline.",
    topic: "pql",
  },
  bad_argument: {
    code: "bad_argument",
    construct: "wrong argument shape",
    reason: "A call that consumes a set of records was handed a number, or the reverse.",
    instead: "`Count` takes a rows expression; `Sum`/`Min`/`Max` take a rows expression plus `field=`.",
    topic: "pql",
  },
  operator_not_allowed: {
    code: "operator_not_allowed",
    construct: "operator on this field kind",
    reason: "A keyed field compares by equality only; an integer field compares by range only.",
    instead: "Use `=` on set/mutex fields and `<`, `<=`, `>`, `>=`, `BETWEEN` on int/decimal fields.",
    topic: "field-kinds",
  },
  too_precise: {
    code: "too_precise",
    construct: "decimal precision",
    reason: "The value carries more digits after the point than the field stores; rounding it would answer a different question.",
    instead: "Write the comparison at the field's own scale.",
    topic: "field-kinds",
  },
  not_pageable: {
    code: "not_pageable",
    construct: "after= on this answer",
    reason: "Only a record listing is pageable; a count or a group set is materialised whole.",
    instead: "Drop `after=`, or ask for `Rows(...)` and page that.",
    topic: "paging",
  },
  query_too_deep: {
    code: "query_too_deep",
    construct: "nesting depth",
    reason: "The query nests deeper than the planner's fixed limit — a bound that keeps the plan stack finite.",
    instead: "Flatten the set algebra: `Union(a, Union(b, c))` is `Union(a, b, c)`.",
    topic: "pql",
  },
  unknown_table: {
    code: "unknown_table",
    construct: "unknown table",
    reason: "No table by that name exists in this deployment. Table names are a flat global namespace inside one process.",
    instead: "Pick one from the schema, or create it with `POST /table/{t}?engine=…` (admin).",
    topic: "schema",
  },
  unknown_field: {
    code: "unknown_field",
    construct: "unknown field",
    reason: "The table exists; the field does not.",
    instead: "Pick one from the schema, or declare it with `POST /table/{t}/field/{f}?kind=…` (admin).",
    topic: "schema",
  },
};

/** Codes that are refusals — designed states — rather than failures. */
const REFUSAL_CODES = new Set(Object.values(REFUSALS).map((r) => r.code));

export function isRefusal(code: string | undefined): boolean {
  return !!code && REFUSAL_CODES.has(code) && code !== "parse_error";
}

/**
 * `sql_unsupported` is one code covering several constructs, because a client
 * retries none of them. The UI still wants to name the construct, so the server's
 * message -- which does name it -- selects the refined entry.
 */
const REFINEMENTS: Array<{ code: string; match: RegExp; key: string }> = [
  { code: "sql_unsupported", match: /\bhaving\b/i, key: "sql_unsupported__having" },
  { code: "sql_unsupported", match: /\boffset\b/i, key: "sql_unsupported__offset" },
  { code: "sql_unsupported", match: /\bsubquery\b|\bcte\b/i, key: "sql_unsupported__subquery" },
  { code: "sql_unsupported", match: /\bwindow\b/i, key: "sql_unsupported__window" },
  { code: "sql_unsupported", match: /\bdistinct\b/i, key: "sql_unsupported__multi_distinct" },
  { code: "sql_unsupported", match: /\bexpression\b/i, key: "sql_unsupported__expression" },
];

/** Resolve a wire error to a catalogue entry. */
export function lookupRefusal(code: string, message?: string): Refusal | undefined {
  if (message) {
    const refined = REFINEMENTS.find((r) => r.code === code && r.match.test(message));
    if (refined) return REFUSALS[refined.key];
  }
  return REFUSALS[code];
}
