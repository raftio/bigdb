import { Badge } from "@/components/badges/status-badge";
import { Tooltip } from "@/components/ui/tooltip";
import type { FieldInfo, FieldKind } from "@/lib/api/types";

/**
 * The kind is what decides the row a value goes into, so it decides which
 * operators are legal on the field. Showing it next to the name is what stops
 * someone writing `> "GB"` and getting an `operator_not_allowed`.
 */
const KIND: Record<FieldKind, { ops: string; why: string }> = {
  set: { ops: "=", why: "One row per distinct value; a record may carry many. Compared by equality only." },
  mutex: { ops: "=", why: "One row per distinct value, and a record carries exactly one. Equality only." },
  bool: { ops: "=", why: "Two rows. Equality only." },
  int: { ops: "< <= > >= BETWEEN", why: "Bit-sliced integer. Ranges are arithmetic over bit planes, not a scan." },
  signed_int: { ops: "< <= > >= BETWEEN", why: "Bit-sliced, stored under an offset-binary bias so the sign never reaches the arithmetic." },
  decimal: { ops: "< <= > >= BETWEEN", why: "Bit-sliced and unsigned, with a scale. A value with more digits than the scale is refused, not rounded." },
  time_quantum: { ops: "BETWEEN", why: "Writes one view per granularity. A window is a key plus bounds against the same column." },
};

export function FieldKindBadge({ field }: { field: FieldInfo }) {
  const k = KIND[field.kind];
  return (
    <Tooltip content={<div className="space-y-1"><div>{k.why}</div><div className="font-mono text-2xs text-fg-faint">operators: {k.ops}</div></div>}>
      <span className="inline-flex items-center gap-1">
        <Badge tone={field.kind === "set" || field.kind === "mutex" || field.kind === "bool" ? "accent" : "neutral"}>
          {field.kind}
        </Badge>
        {field.bit_depth > 0 && <span className="font-mono text-2xs text-fg-faint">{field.bit_depth}b</span>}
        {field.scale ? <span className="font-mono text-2xs text-fg-faint">scale {field.scale}</span> : null}
        {field.granularity?.length ? (
          <span className="font-mono text-2xs text-fg-faint">views {field.granularity.join("")}</span>
        ) : null}
      </span>
    </Tooltip>
  );
}
