import { StreamLanguage, LanguageSupport, HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { tags as t } from "@lezer/highlight";

/**
 * Two grammars, highlighted on their own terms.
 *
 * The SQL mode deliberately marks refused keywords (JOIN, HAVING, OFFSET, …) as
 * `invalid` so the editor says "this is not a thing here" before the request is
 * ever sent. The PQL mode distinguishes rows-producing calls from
 * answer-producing calls, because that distinction is the whole type system.
 */

const SQL_KEYWORDS = new Set([
  "select", "from", "where", "group", "by", "order", "limit", "asc", "desc",
  "and", "or", "not", "in", "between", "distinct", "as", "create", "table", "format",
]);

/** Constructs the planner refuses by name. Rendered as invalid, not as keywords. */
const SQL_REFUSED = new Set([
  "join", "inner", "outer", "left", "right", "full", "cross", "on", "using",
  "having", "offset", "union", "with", "over", "partition", "null", "nulls",
  "insert", "update", "delete", "alter", "truncate", "case", "when", "then", "else", "end",
]);

const SQL_FUNCS = new Set(["count", "sum", "min", "max", "avg", "quantile", "median"]);

export const sqlMode = StreamLanguage.define({
  name: "bigsql",
  token(stream) {
    if (stream.eatSpace()) return null;
    if (stream.match(/^--.*/)) return "comment";
    if (stream.match(/^'([^'\\]|\\.)*'?/)) return "string";
    if (stream.match(/^\d+(\.\d+)?/)) return "number";
    if (stream.match(/^(<=|>=|!=|<>|=|<|>|\*|,|\(|\)|\.)/)) return "operator";
    const w = stream.match(/^[A-Za-z_]\w*/);
    if (w && typeof w !== "boolean") {
      const s = w[0].toLowerCase();
      if (SQL_REFUSED.has(s)) return "invalid";
      if (SQL_KEYWORDS.has(s)) return "keyword";
      if (SQL_FUNCS.has(s)) return "function";
      return "variableName";
    }
    stream.next();
    return null;
  },
});

/** Calls that produce a set of records. */
const PQL_ROWS = new Set(["Row", "All", "Not", "Intersect", "Union", "Difference"]);
/** Calls that produce an answer. */
const PQL_ANSWER = new Set(["Count", "Sum", "Min", "Max", "TopN", "GroupBy", "Distinct", "Rows", "Project"]);

export const pqlMode = StreamLanguage.define({
  name: "pql",
  token(stream) {
    if (stream.eatSpace()) return null;
    if (stream.match(/^#.*/)) return "comment";
    if (stream.match(/^"([^"\\]|\\.)*"?/)) return "string";
    if (stream.match(/^-?\d+(\.\d+)?/)) return "number";
    const call = stream.match(/^[A-Za-z_]\w*(?=\s*\()/);
    if (call && typeof call !== "boolean") {
      const s = call[0];
      if (PQL_ROWS.has(s)) return "typeName";
      if (PQL_ANSWER.has(s)) return "function";
      return "invalid";
    }
    if (stream.match(/^[A-Za-z_]\w*(?=\s*=)/)) return "propertyName";
    if (stream.match(/^(<=|>=|!=|=|<|>|,|\(|\))/)) return "operator";
    if (stream.match(/^[A-Za-z_]\w*/)) return "variableName";
    stream.next();
    return null;
  },
});

/** One palette for both grammars, wired to the theme tokens. */
export const bigHighlight = HighlightStyle.define([
  { tag: t.keyword, color: "hsl(var(--accent))", fontWeight: "500" },
  { tag: t.function(t.variableName), color: "hsl(var(--accent))" },
  { tag: t.typeName, color: "hsl(var(--viz-p99))" },
  { tag: t.propertyName, color: "hsl(var(--fg-muted))" },
  { tag: t.string, color: "hsl(var(--healthy))" },
  { tag: t.number, color: "hsl(var(--degraded))" },
  { tag: t.comment, color: "hsl(var(--fg-faint))", fontStyle: "italic" },
  { tag: t.operator, color: "hsl(var(--fg-faint))" },
  { tag: t.variableName, color: "hsl(var(--fg))" },
  // A construct this engine refuses, marked before it is ever sent.
  { tag: t.invalid, color: "hsl(var(--refused))", textDecoration: "underline wavy hsl(var(--refused) / 0.7)" },
]);

export const sqlSupport = () => new LanguageSupport(sqlMode, [syntaxHighlighting(bigHighlight)]);
export const pqlSupport = () => new LanguageSupport(pqlMode, [syntaxHighlighting(bigHighlight)]);
