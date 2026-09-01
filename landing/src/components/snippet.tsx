import * as React from "react";
import { cn } from "@/lib/cn";

/**
 * One copyable block, used by the install band and by the docs pages.
 *
 * `prompt` is what precedes the first line and is dropped from the clipboard: a reader copies a
 * command, never the `$` in front of it. `copy={false}` is for a block that is a server's
 * answer rather than something to run — copying that back into a terminal is not a thing
 * anyone wants to do.
 */
export function Snippet({ code, prompt, copy = true, className }: {
  code: string; prompt?: string; copy?: boolean; className?: string;
}) {
  const [copied, setCopied] = React.useState(false);
  /* A wrapped command is one command: the continuation is for the eye, not for the shell. */
  const plain = code.replace(/\\\n\s*/g, " ");
  return (
    <div className={cn("mb-2.5 flex items-start gap-3 rounded border border-line bg-surface px-3 py-2.5", className)}>
      <code className="min-w-0 flex-1 overflow-x-auto whitespace-pre text-sm leading-relaxed text-fg">
        {prompt && <span className="text-fg-faint">{prompt}</span>}{code}
      </code>
      {copy && (
        <button
          onClick={() => { navigator.clipboard?.writeText(plain); setCopied(true); setTimeout(() => setCopied(false), 1400); }}
          className={cn(
            "shrink-0 rounded-sm border px-2 py-1 font-mono text-xs transition-colors duration-fast",
            copied ? "border-accent-line text-accent" : "border-line text-fg-faint hover:border-line-strong hover:text-fg",
          )}
        >
          {copied ? "copied" : "copy"}
        </button>
      )}
    </div>
  );
}
