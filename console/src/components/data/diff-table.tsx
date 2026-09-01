"use client";
import { Check, X, Minus } from "lucide-react";
import { cn } from "@/lib/cn";
import { num } from "@/lib/format";
import type { VerifyResult } from "@/lib/api/types";

/**
 * /verify asks whether the copies of every range agree. The answer is a diff,
 * so it renders as one: the owner's checksum is the reference column and each
 * replica is a cell that either matches it or does not — with the number of
 * records it is behind, which is what `POST /repair` would have to move.
 */
export function AgreementTable({ result, className }: { result: VerifyResult; className?: string }) {
  const nodes = Array.from(new Set(result.rows.flatMap((r) => r.replicas.map((x) => x.node)))).sort();

  return (
    <div className={cn("overflow-x-auto", className)}>
      <table className="w-full border-collapse text-base">
        <thead>
          <tr className="border-b border-line bg-surface-sunken text-2xs uppercase tracking-wider text-fg-faint">
            <th className="px-2 py-1.5 text-left font-medium">range</th>
            <th className="px-2 py-1.5 text-left font-medium">fragment</th>
            <th className="px-2 py-1.5 text-left font-medium">owner</th>
            {nodes.map((n) => <th key={n} className="px-2 py-1.5 text-left font-medium font-mono normal-case">{n}</th>)}
          </tr>
        </thead>
        <tbody>
          {result.rows.map((row, i) => (
            <tr key={i} className={cn("border-b border-line/60", row.replicas.some((r) => !r.agrees) && "bg-degraded-soft/40")}>
              <td className="px-2 py-1.5 font-mono text-sm">{row.range}</td>
              <td className="px-2 py-1.5 font-mono text-sm text-fg-muted">{row.field}</td>
              <td className="px-2 py-1.5 font-mono text-sm text-accent">{row.owner_checksum}</td>
              {nodes.map((n) => {
                const r = row.replicas.find((x) => x.node === n);
                if (!r) return <td key={n} className="px-2 py-1.5 text-fg-faint"><Minus className="size-3" aria-hidden /><span className="sr-only">not a replica</span></td>;
                return (
                  <td key={n} className="px-2 py-1.5">
                    <div className="flex items-center gap-1.5">
                      {r.agrees
                        ? <Check className="size-3 shrink-0 text-healthy" aria-hidden />
                        : <X className="size-3 shrink-0 text-degraded" aria-hidden />}
                      <span className={cn("font-mono text-sm", r.agrees ? "text-fg-muted" : "text-degraded")}>
                        {r.checksum}
                      </span>
                      {!r.agrees && r.behind_records >= 0 && (
                        <span className="font-mono text-2xs text-fg-faint">−{num(r.behind_records)}</span>
                      )}
                      <span className="sr-only">{r.agrees ? "agrees" : "differs"}</span>
                    </div>
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
