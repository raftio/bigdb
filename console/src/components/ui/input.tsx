"use client";
import * as React from "react";
import { cn } from "@/lib/cn";

export const Input = React.forwardRef<HTMLInputElement, React.InputHTMLAttributes<HTMLInputElement>>(
  ({ className, ...props }, ref) => (
    <input
      ref={ref}
      className={cn(
        "h-8 w-full rounded border border-line bg-surface px-2.5 text-base text-fg",
        "placeholder:text-fg-faint transition-colors duration-fast",
        "hover:border-line-strong focus:border-accent",
        "disabled:cursor-not-allowed disabled:opacity-50",
        className,
      )}
      {...props}
    />
  ),
);
Input.displayName = "Input";

export const Textarea = React.forwardRef<HTMLTextAreaElement, React.TextareaHTMLAttributes<HTMLTextAreaElement>>(
  ({ className, ...props }, ref) => (
    <textarea
      ref={ref}
      className={cn(
        "w-full rounded border border-line bg-surface px-2.5 py-2 text-base font-mono text-fg",
        "placeholder:text-fg-faint transition-colors duration-fast resize-y",
        "hover:border-line-strong focus:border-accent",
        className,
      )}
      {...props}
    />
  ),
);
Textarea.displayName = "Textarea";

export function Field({ label, hint, children, htmlFor }: {
  label: string; hint?: React.ReactNode; children: React.ReactNode; htmlFor?: string;
}) {
  return (
    <div className="space-y-1.5">
      <label htmlFor={htmlFor} className="block text-sm font-medium text-fg-muted">{label}</label>
      {children}
      {hint && <p className="text-xs text-fg-faint leading-relaxed">{hint}</p>}
    </div>
  );
}
