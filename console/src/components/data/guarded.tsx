"use client";
import type { Role } from "@/lib/api/types";
import { requires, denialReason } from "@/lib/api/client";
import { Tooltip } from "@/components/ui/tooltip";

/**
 * A control the current token cannot use is shown, disabled, with the reason —
 * never hidden. Hiding it teaches the wrong shape of the product; disabling it
 * with a sentence teaches the right one, and tells you which token to go get.
 */
export function Guarded({ role, need, children, what }: {
  role: Role; need: Role; children: (allowed: boolean) => React.ReactNode; what?: string;
}) {
  const allowed = requires(role, need);
  if (allowed) return <>{children(true)}</>;
  return (
    <Tooltip content={
      <span>
        {what ? <>{what} requires <code>{need}</code>. </> : null}
        {denialReason(role, need)}
      </span>
    }>
      <span className="inline-flex" tabIndex={0}>{children(false)}</span>
    </Tooltip>
  );
}
