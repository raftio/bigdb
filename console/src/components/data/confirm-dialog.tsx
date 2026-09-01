"use client";
import * as React from "react";
import { AlertTriangle } from "lucide-react";
import { Dialog, DialogContent, DialogBody, DialogFooter, DialogClose } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/cn";

/**
 * Every destructive or cluster-wide action states its blast radius before it
 * asks. Where the action cannot be undone, the confirmation is typed: the user
 * has to write the thing's own name, which is the only guard that survives a
 * muscle-memory click.
 */
export function ConfirmDialog({
  open, onOpenChange, title, blastRadius, detail, confirmWord, confirmLabel = "Confirm",
  onConfirm, tone = "danger", busy,
}: {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  title: string;
  /** What this touches, said plainly. Required — an action with no stated radius has no business here. */
  blastRadius: React.ReactNode;
  detail?: React.ReactNode;
  /** When present, the user must type it exactly. */
  confirmWord?: string;
  confirmLabel?: string;
  onConfirm: () => void | Promise<void>;
  tone?: "danger" | "caution";
  busy?: boolean;
}) {
  const [typed, setTyped] = React.useState("");
  React.useEffect(() => { if (!open) setTyped(""); }, [open]);
  const ok = !confirmWord || typed === confirmWord;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={title}>
        <DialogBody className="space-y-3">
          <div className={cn(
            "flex gap-2.5 rounded border px-3 py-2.5",
            tone === "danger" ? "border-failed/35 bg-failed-soft/50" : "border-degraded/35 bg-degraded-soft/50",
          )}>
            <AlertTriangle className={cn("mt-0.5 size-3.5 shrink-0", tone === "danger" ? "text-failed" : "text-degraded")} aria-hidden />
            <div className="min-w-0 space-y-1">
              <div className="text-2xs font-medium uppercase tracking-wider text-fg-faint">blast radius</div>
              <div className="text-base leading-relaxed text-fg">{blastRadius}</div>
            </div>
          </div>
          {detail && <div className="text-base leading-relaxed text-fg-muted">{detail}</div>}
          {confirmWord && (
            <div className="space-y-1.5">
              <label htmlFor="confirm-word" className="block text-sm text-fg-muted">
                Type <code className="text-fg">{confirmWord}</code> to confirm
              </label>
              <Input
                id="confirm-word" value={typed} autoFocus autoComplete="off" spellCheck={false}
                className="font-mono" onChange={(e) => setTyped(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter" && ok) onConfirm(); }}
                aria-invalid={typed.length > 0 && !ok}
              />
            </div>
          )}
        </DialogBody>
        <DialogFooter>
          <DialogClose asChild><Button variant="ghost">Cancel</Button></DialogClose>
          <Button variant={tone === "danger" ? "danger" : "primary"} disabled={!ok || busy} onClick={() => onConfirm()}>
            {busy ? "Working…" : confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
