import * as React from "react";
import { Ban } from "lucide-react";
import { Band, BandHead } from "@/components/section";
import { RefusalPanel } from "@/components/refusal-panel";
import { Badge } from "@/components/badges/status-badge";
import { REFUSALS } from "@/lib/refusals";
import { ApiError } from "@/lib/types";

/**
 * The refusal section shows the *actual* console component, driven by the
 * *actual* catalogue, with the *actual* stable codes. Nothing here is a mockup:
 * paste the same statement into the workbench and you get this panel.
 */
const SHOWN = [
  "sql_no_joins",
  "sql_unsupported__having",
  "sql_unsupported__offset",
  "sql_projection_unsupported",
  "sql_no_nulls",
  "sql_union",
  "sql_read_only",
  "not_pageable",
] as const;

const DEMO_QUERY = `SELECT a.country, count(*)
FROM events a JOIN sessions b ON a.id = b.id
GROUP BY a.country`;

const DEMO_ERROR = new ApiError(
  400,
  { code: "sql_no_joins", message: "no joins: a fact is one bit at (row, record)" },
  [DEMO_QUERY.indexOf("JOIN"), DEMO_QUERY.indexOf("JOIN") + 4],
);

export function RefusalsSection() {
  const [query, setQuery] = React.useState(DEMO_QUERY);
  const [rewritten, setRewritten] = React.useState(false);

  return (
    <Band id="refusals">
      <BandHead
        title="Refusals are the feature"
        lede="Anything this engine cannot answer is refused by name, at parse time, with a stable code and a sentence saying what exists instead. Never a red toast that says “Error”."
      />

      <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)] lg:gap-12">
        <div>
          {rewritten ? (
            <div className="rounded-lg border border-healthy/40 bg-healthy-soft/40 p-4">
              <p className="text-base text-fg">
                That is the rewrite, applied. The join is gone and so are the aliases it existed for —
                which is what “what exists instead” means when the alternative is mechanical.
              </p>
              <pre className="mt-3 overflow-x-auto rounded border border-line bg-surface px-2.5 py-2 font-mono text-sm text-fg"><code>{query}</code></pre>
              <button onClick={() => { setQuery(DEMO_QUERY); setRewritten(false); }}
                className="mt-3 font-mono text-xs text-accent underline underline-offset-2">
                show the refusal again
              </button>
            </div>
          ) : (
            <RefusalPanel
              error={DEMO_ERROR}
              query={query}
              onRewrite={(next) => { setQuery(next); setRewritten(true); }}
            />
          )}
          <p className="mt-3 text-sm leading-relaxed text-fg-faint">
            This is the console component, not a picture of it. Try “Apply”.
          </p>
        </div>

        <ul className="border-t border-line">
          {SHOWN.map((key) => {
            const r = REFUSALS[key];
            return (
              <li key={key} className="grid grid-cols-[13px_minmax(0,1fr)] gap-3.5 border-b border-line py-3.5">
                <Ban className="mt-1 size-3 shrink-0 text-refused" aria-hidden />
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <strong className="font-mono text-md font-medium text-fg">{r.construct}</strong>
                    <Badge tone="refused">{r.code}</Badge>
                  </div>
                  <p className="mt-1 text-base leading-relaxed text-fg-muted">{r.instead}</p>
                </div>
              </li>
            );
          })}
        </ul>
      </div>
    </Band>
  );
}
