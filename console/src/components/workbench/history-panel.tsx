"use client";
import { Ban, Pin, PinOff, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/badges/status-badge";
import { EmptyState } from "@/components/data/states";
import { Tooltip } from "@/components/ui/tooltip";
import { relTime, us } from "@/lib/format";
import { isRefusal } from "@/lib/refusals";
import { cn } from "@/lib/cn";
import type { HistoryEntry } from "@/lib/hooks/use-history";

/**
 * History keeps refusals as first-class entries rather than discarding them.
 * A refused query is often the most useful thing in the list: it is the moment
 * someone learned what the engine does not do, and it is worth pinning.
 */
export function HistoryPanel({ entries, onPick, onTogglePin, onClear }: {
  entries: HistoryEntry[];
  onPick: (e: HistoryEntry) => void;
  onTogglePin: (id: string) => void;
  onClear: () => void;
}) {
  const pinned = entries.filter((e) => e.pinned);
  const rest = entries.filter((e) => !e.pinned);

  if (!entries.length) {
    return <EmptyState title="No queries yet"
      description="Run something and it lands here. Pin the ones you keep coming back to; they survive a clear." />;
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="min-h-0 flex-1 overflow-y-auto">
        {pinned.length > 0 && (
          <Section label={`pinned (${pinned.length})`}>
            {pinned.map((e) => <Row key={e.id} entry={e} onPick={onPick} onTogglePin={onTogglePin} />)}
          </Section>
        )}
        {rest.length > 0 && (
          <Section label={`recent (${rest.length})`}>
            {rest.map((e) => <Row key={e.id} entry={e} onPick={onPick} onTogglePin={onTogglePin} />)}
          </Section>
        )}
      </div>
      <div className="shrink-0 border-t border-line p-2">
        <Button size="xs" variant="ghost" className="w-full" onClick={onClear} disabled={!rest.length}>
          <Trash2 aria-hidden /> Clear unpinned
        </Button>
      </div>
    </div>
  );
}

function Section({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="sticky top-0 z-10 bg-surface-sunken px-2.5 py-1 text-2xs font-medium uppercase tracking-wider text-fg-faint">
        {label}
      </div>
      <ul>{children}</ul>
    </div>
  );
}

function Row({ entry, onPick, onTogglePin }: {
  entry: HistoryEntry; onPick: (e: HistoryEntry) => void; onTogglePin: (id: string) => void;
}) {
  const refused = isRefusal(entry.code ?? undefined);
  return (
    <li className="group border-b border-line/60">
      <div className="flex items-start gap-1.5 px-2 py-1.5 hover:bg-surface-raised">
        <button onClick={() => onPick(entry)} className="min-w-0 flex-1 text-left">
          <div className="flex items-center gap-1.5">
            <Badge tone={entry.surface === "sql" ? "neutral" : "accent"}>{entry.surface}</Badge>
            {entry.table && <span className="truncate font-mono text-2xs text-fg-faint">{entry.table}</span>}
            {entry.code && (
              <Badge tone={refused ? "refused" : "failed"}>
                {refused && <Ban className="size-2.5" aria-hidden />}{entry.code}
              </Badge>
            )}
            <span className="ml-auto shrink-0 font-mono text-2xs text-fg-faint">
              {entry.duration_us ? us(entry.duration_us) : relTime(new Date(entry.at).toISOString())}
            </span>
          </div>
          <code className={cn("mt-1 line-clamp-2 block text-sm leading-snug",
            refused ? "text-refused/90" : entry.code ? "text-failed/90" : "text-fg-muted")}>
            {entry.text.replace(/\s+/g, " ").trim()}
          </code>
        </button>
        <Tooltip content={entry.pinned ? "Unpin" : "Pin"}>
          <Button size="iconSm" variant="ghost"
            className={cn(!entry.pinned && "opacity-0 group-hover:opacity-100 focus:opacity-100")}
            aria-label={entry.pinned ? "Unpin query" : "Pin query"}
            onClick={() => onTogglePin(entry.id)}>
            {entry.pinned ? <PinOff aria-hidden /> : <Pin aria-hidden />}
          </Button>
        </Tooltip>
      </div>
    </li>
  );
}
