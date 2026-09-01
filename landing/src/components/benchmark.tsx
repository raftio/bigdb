import { Band, BandHead } from "@/components/section";
import { Badge } from "@/components/badges/status-badge";
import { EngineBadge } from "@/components/badges/engine-badge";
import type { TableEngine } from "@/lib/types";

/**
 * Measured, and the provenance is part of the table rather than a footnote.
 *
 * Produced by `./scripts/bench olap 2000000` on the rig named below, at the
 * commit named below. Every answer was checked against a ground truth computed
 * without any engine, so a wrong number aborts the run instead of appearing
 * here; query timings are medians of three and the load is a single shot.
 *
 * The figures that stood here before were placeholders, and they were not
 * merely imprecise — they were backwards. They had bitmap+columnar loading
 * fastest of the three when it is by some way the slowest, and bitmap smallest
 * on disk when columnar is. That is what a table shaped like the result you
 * expect is worth.
 */
const RIG = "DO droplet · 2 vCPU · 3.8 GiB · Ubuntu 24.04 · rustc 1.88";
const COMMIT = "7adb7f7";

const ROWS: Array<{
  engine: string; badge: TableEngine;
  load: string; filter: string; topn: string; group: string; disk: string;
  best?: Array<"filter" | "topn" | "group" | "disk" | "load">;
}> = [
  {
    engine: "bigdb", badge: "bitmap",
    load: "11.3 s", filter: "82 ms", topn: "2.9 ms", group: "21 ms", disk: "14.5 MiB",
    best: ["topn"],
  },
  {
    engine: "bigdb", badge: "columnar",
    load: "9.4 s", filter: "669 ms", topn: "543 ms", group: "652 ms", disk: "13.1 MiB",
    best: ["load", "disk"],
  },
  {
    engine: "bigdb", badge: "bitmap+columnar",
    load: "20.6 s", filter: "66 ms", topn: "3.8 ms", group: "22 ms", disk: "49.7 MiB",
    best: ["filter"],
  },
];

const COLS = [
  { key: "load", head: "Load" },
  { key: "filter", head: "count, 3 predicates" },
  { key: "topn", head: "TopN (n = 10)" },
  { key: "group", head: "GroupBy, 256 groups" },
  { key: "disk", head: "On disk" },
] as const;

export function Benchmark() {
  return (
    <Band id="benchmark">
      <BandHead
        title="Where the bits pay for themselves"
        lede="Two million records, one dataset, five questions. The engine is fixed at CREATE and decides what a table writes for every fact, so this compares bigdb's three against each other — the choice a table actually faces."
        aside={<Badge tone="healthy">measured</Badge>}
      />

      <div className="overflow-x-auto rounded-lg border border-line bg-surface">
        <table className="w-full min-w-[760px] border-collapse">
          <thead>
            <tr className="border-b border-line bg-surface-raised">
              <th scope="col" className="px-4 py-2.5 text-left text-xs font-medium text-fg-faint">Engine</th>
              {COLS.map((c) => (
                <th key={c.key} scope="col" className="px-4 py-2.5 text-right text-xs font-medium text-fg-faint">{c.head}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {ROWS.map((r) => (
              <tr key={r.badge} className="border-b border-line last:border-b-0">
                <th scope="row" className="px-4 py-2.5 text-left text-base font-normal">
                  <span className="flex items-center gap-2 text-fg">
                    {r.engine}
                    <EngineBadge engine={r.badge} />
                  </span>
                </th>
                {COLS.map((c) => {
                  const best = r.best?.includes(c.key as never);
                  return (
                    <td key={c.key}
                      className={`px-4 py-2.5 text-right font-mono text-base tabular ${
                        best ? "text-accent" : "text-fg-muted"
                      }`}>
                      {r[c.key]}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <div className="mt-3.5 flex flex-wrap gap-x-6 gap-y-1.5 text-sm text-fg-faint">
        <span><b className="font-medium text-fg-muted">bitmap</b> — predicates never leave bit space.</span>
        <span><b className="font-medium text-fg-muted">columnar</b> — cheapest to write and to store, and it scans for every filter.</span>
        <span><b className="font-medium text-fg-muted">bitmap+columnar</b> — writes both, and the load row is what both cost.</span>
      </div>

      <div className="mt-5 space-y-2 border-t border-line pt-4 text-sm leading-relaxed text-fg-faint">
        <p>
          <b className="font-medium text-fg-muted">GroupBy is a tie.</b>{" "}
          21 ms and 22 ms are three percent apart on a two-core host, which is
          inside its noise — neither is highlighted, because a winner drawn from
          a gap that small is a winner drawn from nothing.
        </p>
        <p>
          <b className="font-medium text-fg-muted">Loaded as a library, not over the wire.</b>{" "}
          The load column calls the engine directly. Over HTTP — a line parsed
          per fact, one commit per request, a round trip per 7 MiB — the same
          corpus goes in at about 430,000 records/s on this rig with{" "}
          <code className="text-fg-muted">bigi --in-flight 2</code> against a
          bitmap table. That is a different road and is measured by{" "}
          <code className="text-fg-muted">scripts/bench server</code>.
        </p>
        <p>
          <b className="font-medium text-fg-muted">No rival is shown, and that is not an omission.</b>{" "}
          DuckDB, DataFusion and ClickHouse are peers the harness knows how to
          run — <code className="text-fg-muted">BENCH_PEERS=olap-peers</code> —
          and none of them was compiled into this run. A column store column
          invented to sit under these would be the one number on this page
          nobody had measured.
        </p>
        <p>
          <b className="font-medium text-fg-muted">Provenance.</b>{" "}
          <code className="text-fg-muted">./scripts/bench olap 2000000</code> at{" "}
          <code className="text-fg-muted">{COMMIT}</code> on {RIG}. Dense ids,
          256 categories, 20 countries, amounts under 2²⁰; the questions are{" "}
          <code className="text-fg-muted">amount ≥ 786432</code>, one country,{" "}
          <code className="text-fg-muted">active</code>, top 10. Query timings
          are medians of three, the load is a single shot, and every answer was
          checked against a ground truth computed without any engine. One node:
          one process, one file, no fan-out and no network.
        </p>
      </div>
    </Band>
  );
}
