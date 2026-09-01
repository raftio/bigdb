import { Band, BandHead } from "@/components/section";

/**
 * Six layers, read downward. Nothing calls upward and nothing skips a layer.
 *
 * Drawn in HTML rather than SVG: these captions are prose, and SVG <text> does
 * not wrap — the first version spilled every long caption across the box beside
 * it. HTML also gives selectable text, a real reading order for a screen
 * reader, and a layout that can drop to one column on a phone instead of
 * shrinking a fixed viewBox until nothing is legible.
 */
const LAYERS: Array<{
  label: string;
  boxes: Array<{ name: string; sub: string }>;
  /** The storage layer is two wide boxes rather than three. */
  wide?: boolean;
}> = [
  {
    label: "EDGE",
    boxes: [
      { name: "Reverse proxy", sub: "TLS terminates here; bigd speaks HTTP/1.1" },
      { name: "Bearer token", sub: "verified once, from a file the process reads" },
      { name: "Role", sub: "read · write · admin — verbs, not rows" },
    ],
  },
  {
    label: "CLUSTER",
    boxes: [
      { name: "Ranges", sub: "from a config file, not from a metric" },
      { name: "One copy serves a range", sub: "failover ≈ 1 s; never a stale read" },
      { name: "Schema leader", sub: "owns every row key" },
    ],
  },
  {
    label: "FACADE",
    boxes: [
      { name: "Routing", sub: "to the owner of the range, or 503 naming its shards" },
      { name: "Deadline", sub: "the client closing the connection is the signal" },
      { name: "Structured log", sub: "one JSON line per request, with its id" },
    ],
  },
  {
    label: "PLANNER",
    boxes: [
      { name: "Two surfaces, one planner", sub: "SQL and PQL lower to the same plan" },
      { name: "Refusal", sub: "by name, at parse time, with a stable code" },
      { name: "Fan-out and merge", sub: "one plan per shard, merged once" },
    ],
  },
  {
    label: "DATA",
    boxes: [
      { name: "Bitmap", sub: "a filter is an intersection; a count is a popcount" },
      { name: "Bit-sliced integers", sub: "sums and ranges without decoding a value" },
      { name: "Engine per table", sub: "bitmap · bitmap+columnar · columnar" },
    ],
  },
  {
    label: "STORAGE",
    wide: true,
    boxes: [
      { name: "No WAL", sub: "pages → fsync → flip the meta page → fsync" },
      { name: "One file, one tenant", sub: "a fragment is (table, field, view, shard)" },
    ],
  },
];

export function Architecture() {
  return (
    <Band id="architecture">
      <BandHead
        title="Six layers, one direction"
        lede="A request enters at the top and descends. Nothing calls upward, and nothing skips a layer."
      />
      <div className="rounded-lg border border-line bg-surface p-4 md:p-6">
        <ol className="list-none">
          {LAYERS.map((layer, i) => (
            <li key={layer.label}>
              <h3 className="font-mono text-2xs uppercase tracking-[0.08em] text-fg-faint">{layer.label}</h3>
              <div className="mb-3 mt-1.5 border-t border-line" />
              <div className={`grid gap-2.5 ${layer.wide ? "sm:grid-cols-2" : "sm:grid-cols-2 lg:grid-cols-3"}`}>
                {layer.boxes.map((b) => (
                  <div key={b.name} className="rounded border border-line-strong bg-surface-raised px-4 py-3">
                    <div className="text-base leading-snug text-fg">{b.name}</div>
                    <div className="mt-1 font-mono text-2xs leading-relaxed text-fg-faint">{b.sub}</div>
                  </div>
                ))}
              </div>

              {i === 0 && <TrustBoundary />}
              {i < LAYERS.length - 1 && <Arrow />}
            </li>
          ))}
        </ol>
      </div>
    </Band>
  );
}

/** Everything below this line is talking to a caller who already has a role. */
function TrustBoundary() {
  return (
    <div className="mt-5 flex items-center gap-3" role="separator" aria-label="Trust boundary">
      <span className="font-mono text-2xs leading-relaxed text-accent">
        TRUST BOUNDARY — below this line a request is who it says it is, and carries a role
      </span>
      {/* The rule is decoration; below sm the caption needs the whole width. */}
      <span aria-hidden className="hidden h-px flex-1 border-t border-dashed border-accent-line sm:block" />
    </div>
  );
}

function Arrow() {
  return (
    <div className="flex h-8 items-center justify-center" aria-hidden>
      <svg width="9" height="22" viewBox="0 0 9 22">
        <path d="M4.5 0v17M1 15l3.5 4L8 15" fill="none" stroke="hsl(var(--border-strong))" strokeWidth="1.3" />
      </svg>
    </div>
  );
}
