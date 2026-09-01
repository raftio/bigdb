"use client";
import * as D from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { cn } from "@/lib/cn";

export const Dialog = D.Root;
export const DialogTrigger = D.Trigger;
export const DialogClose = D.Close;

export function DialogContent({ children, className, title, description }: {
  children: React.ReactNode; className?: string; title: string; description?: React.ReactNode;
}) {
  return (
    <D.Portal>
      <D.Overlay className="fixed inset-0 z-50 bg-black/55 animate-fade-in" />
      <D.Content
        className={cn(
          "fixed left-1/2 top-1/2 z-50 w-full max-w-lg -translate-x-1/2 -translate-y-1/2",
          "rounded-lg border border-line bg-overlay shadow-e3 animate-slide-up outline-none",
          className,
        )}
      >
        <div className="flex items-start justify-between gap-4 border-b border-line px-4 py-3">
          <div className="min-w-0">
            <D.Title className="text-md font-semibold text-fg">{title}</D.Title>
            {description && <D.Description className="mt-1 text-sm text-fg-muted leading-relaxed">{description}</D.Description>}
          </div>
          <D.Close className="shrink-0 rounded p-1 text-fg-faint hover:bg-surface-raised hover:text-fg" aria-label="Close">
            <X className="size-3.5" />
          </D.Close>
        </div>
        {children}
      </D.Content>
    </D.Portal>
  );
}

export function DialogFooter({ children }: { children: React.ReactNode }) {
  return <div className="flex items-center justify-end gap-2 border-t border-line px-4 py-3">{children}</div>;
}

export function DialogBody({ children, className }: { children: React.ReactNode; className?: string }) {
  return <div className={cn("px-4 py-4", className)}>{children}</div>;
}
