import type { CompletionContext, CompletionResult, Completion } from "@codemirror/autocomplete";
import type { SchemaResponse, FieldKind } from "@/lib/api/types";

/**
 * Completion comes from GET /schema and nothing else — the console never
 * suggests a table, a field or a call the server would not accept. The engine
 * badge rides along in the detail line, because which engine a table uses is
 * what decides whether the query you are about to write is cheap.
 */

const KIND_HINT: Record<FieldKind, string> = {
  set: "= only", mutex: "= only, one row per record", bool: "= true/false",
  int: "range", signed_int: "range, biased", decimal: "range, scaled", time_quantum: "BETWEEN",
};

function fieldCompletions(schema: SchemaResponse, table?: string): Completion[] {
  const tables = table ? schema.tables.filter((t) => t.name === table) : schema.tables;
  return tables.flatMap((t) =>
    t.fields.map<Completion>((f) => ({
      label: f.name,
      type: f.kind === "set" || f.kind === "mutex" ? "property" : "variable",
      detail: f.kind + (f.bit_depth ? `(${f.bit_depth})` : "") + (f.scale ? ` scale=${f.scale}` : ""),
      info: `${t.name} · ${t.engine} — ${KIND_HINT[f.kind]}${f.granularity?.length ? ` · views ${f.granularity.join("")}` : ""}`,
      boost: table ? 2 : 0,
    })),
  );
}

function tableCompletions(schema: SchemaResponse): Completion[] {
  return schema.tables.map<Completion>((t) => ({
    label: t.name,
    type: "class",
    detail: t.engine,
    info: `${t.fields.length} fields · engine ${t.engine}`,
  }));
}

const SQL_SNIPPETS: Completion[] = [
  { label: "SELECT", type: "keyword", detail: "single table only" },
  { label: "FROM", type: "keyword" },
  { label: "WHERE", type: "keyword" },
  { label: "GROUP BY", type: "keyword", detail: "one column" },
  { label: "ORDER BY", type: "keyword", detail: "grouped column or the one aggregate" },
  { label: "LIMIT", type: "keyword", detail: "1–10000 when values are selected" },
  { label: "count(*)", type: "function", detail: "popcount, not a scan" },
  { label: "count(DISTINCT )", type: "function", detail: "one keyed column" },
  { label: "sum()", type: "function", detail: "BSI field" },
  { label: "min()", type: "function" },
  { label: "max()", type: "function" },
  { label: "CREATE TABLE", type: "keyword", detail: "admin · no column list" },
];

const PQL_SNIPPETS: Completion[] = [
  { label: "Count", type: "function", detail: "(rows) → one number", info: "A popcount over the bitmap the argument produces." },
  { label: "Union", type: "type", detail: "(rows, rows, …) → rows", info: "Bitmap OR." },
  { label: "Intersect", type: "type", detail: "(rows, rows, …) → rows", info: "Bitmap AND — this is what a filter is." },
  { label: "Difference", type: "type", detail: "(rows, rows) → rows", info: "Bitmap ANDNOT." },
  { label: "Not", type: "type", detail: "(rows) → rows" },
  { label: "Row", type: "type", detail: "(field=value) → rows", info: "One row of the bitmap: every record carrying this value." },
  { label: "All", type: "type", detail: "() → rows" },
  { label: "TopN", type: "function", detail: "(rows, field=, n=) → ranked keys" },
  { label: "GroupBy", type: "function", detail: "(field=, aggregate=Sum(field=…)) → groups" },
  { label: "Distinct", type: "function", detail: "(field=) → keys" },
  { label: "Rows", type: "function", detail: "(rows) → a page of record ids", info: "The only pageable answer: use ?after= and ?limit=." },
  { label: "Sum", type: "function", detail: "(rows, field=) → one number" },
  { label: "Min", type: "function", detail: "(rows, field=) → one number" },
  { label: "Max", type: "function", detail: "(rows, field=) → one number" },
];

export function makeSqlCompletion(schema: SchemaResponse | undefined) {
  return (ctx: CompletionContext): CompletionResult | null => {
    const word = ctx.matchBefore(/[\w*(]+/);
    if (!word && !ctx.explicit) return null;
    const text = ctx.state.doc.toString().slice(0, ctx.pos);
    const table = /\bFROM\s+([A-Za-z_]\w*)/i.exec(text)?.[1];
    const afterFrom = /\bFROM\s+\w*$/i.test(text);
    const options = !schema
      ? SQL_SNIPPETS
      : afterFrom
        ? tableCompletions(schema)
        : [...SQL_SNIPPETS, ...tableCompletions(schema), ...fieldCompletions(schema, table)];
    return { from: word?.from ?? ctx.pos, options, validFor: /^[\w*(]*$/ };
  };
}

export function makePqlCompletion(schema: SchemaResponse | undefined, table: string) {
  return (ctx: CompletionContext): CompletionResult | null => {
    const word = ctx.matchBefore(/[\w]+/);
    if (!word && !ctx.explicit) return null;
    const options = schema ? [...PQL_SNIPPETS, ...fieldCompletions(schema, table)] : PQL_SNIPPETS;
    return { from: word?.from ?? ctx.pos, options, validFor: /^\w*$/ };
  };
}
