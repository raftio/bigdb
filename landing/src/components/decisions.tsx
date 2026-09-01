import { Band, BandHead } from "@/components/section";

/**
 * Each of these closes a door. They are stated here rather than discovered
 * three months into a migration — the same reason the planner refuses by name.
 */
const DECISIONS: Array<{ title: React.ReactNode; body: React.ReactNode }> = [
  {
    title: "Every fact is one bit at (row, record)",
    body: "A filter is a bitmap intersection and a count is a popcount. Nothing is scanned, decoded or materialised to answer a predicate — the engine ANDs machine words and counts the ones that stayed.",
  },
  {
    title: "No write-ahead log",
    body: "A commit writes pages, fsyncs, flips the meta page and fsyncs again. There is no in-between state to recover from: the file is either the old meta or the new one. A log would be a second source of truth to keep honest.",
  },
  {
    title: "Backup, compaction and migration are one operation",
    body: "All three copy the live pages into a fresh file. There is no separate backup path to rot, and a format upgrade is a compaction that happens to write the new page layout.",
  },
  {
    title: "One tenant, one process, one file",
    body: "A deployment is one bigd over one file, with its own token set. Isolation comes from the operating system, not from a scheduler we wrote. Table names are a flat global namespace inside it and are invisible outside it.",
  },
  {
    title: "The engine is chosen at CREATE",
    body: "bitmap · bitmap+columnar · columnar. It decides what the table writes for every fact, and therefore what a query costs. It never changes: changing it means copying the table into a new one.",
  },
  {
    title: "Two query surfaces, one planner",
    body: "PQL asks in the engine's own terms — Count, Union, TopN, GroupBy. SQL is a single-table SELECT that lowers to the same plan. Anything SQL cannot lower it refuses, rather than emulating.",
  },
  {
    title: "Roles are verbs, not rows",
    body: "read, write, admin — bearer tokens from a file the process reads. There are no row filters and no per-column grants. Two audiences means two deployments, and two deployments means two files.",
  },
  {
    title: "The cluster is CP",
    body: "Ranges come from a config file and one copy serves each. Failover is about a second, one schema leader owns every row key, and a read is never answered from a copy known to be stale — it is refused, naming the shards the owner holds.",
  },
  {
    title: "Paging is by cursor, never by offset",
    body: "A skip count shifts when records are inserted under it. Only a record listing is pageable, with an after cursor; a count or a group set is materialised whole, and asking to page one is refused as not_pageable.",
  },
];

export function Decisions() {
  return (
    <Band id="decisions">
      <BandHead
        title="Nine decisions worth stating"
        lede="Each of these closes a door. We would rather you find them here than three months into a migration."
      />
      <div className="grid gap-px overflow-hidden rounded-lg border border-line bg-line sm:grid-cols-2 lg:grid-cols-3">
        {DECISIONS.map((d) => (
          <article key={String(d.title)} className="flex flex-col gap-2.5 bg-surface p-5 pb-6">
            <span aria-hidden className="block size-[9px] rounded-[1px] bg-accent" />
            <h3 className="text-lg font-medium leading-snug tracking-tight text-fg">{d.title}</h3>
            <p className="text-base leading-relaxed text-fg-muted">{d.body}</p>
          </article>
        ))}
      </div>
    </Band>
  );
}
