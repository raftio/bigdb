"use client";
import * as React from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "@/lib/cn";

export interface Column<T> {
  key: string;
  header: React.ReactNode;
  /** Fixed width in px. Dense tables do not reflow while you read them. */
  width?: number;
  align?: "left" | "right";
  mono?: boolean;
  cell: (row: T, index: number) => React.ReactNode;
  sort?: (a: T, b: T) => number;
}

/**
 * A dense table. Rows are 28px, headers are 26px, numbers are right-aligned and
 * tabular. Above `virtualizeAbove` rows it virtualizes; below, it does not —
 * a 12-row table has no business owning a scroll container.
 */
export function DataTable<T>({
  rows, columns, rowKey, onRowClick, selectedKey, className, empty, virtualizeAbove = 60, maxHeight = 520, stickyHeader = true,
}: {
  rows: T[];
  columns: Column<T>[];
  rowKey: (row: T, i: number) => string;
  onRowClick?: (row: T) => void;
  selectedKey?: string;
  className?: string;
  empty?: React.ReactNode;
  virtualizeAbove?: number;
  maxHeight?: number;
  stickyHeader?: boolean;
}) {
  const [sort, setSort] = React.useState<{ key: string; dir: 1 | -1 } | null>(null);
  const parentRef = React.useRef<HTMLDivElement>(null);

  const sorted = React.useMemo(() => {
    if (!sort) return rows;
    const col = columns.find((c) => c.key === sort.key);
    if (!col?.sort) return rows;
    return [...rows].sort((a, b) => col.sort!(a, b) * sort.dir);
  }, [rows, sort, columns]);

  const virtual = sorted.length > virtualizeAbove;
  const virtualizer = useVirtualizer({
    count: sorted.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 28,
    overscan: 14,
    enabled: virtual,
  });

  const grid = columns.map((c) => (c.width ? `${c.width}px` : "minmax(80px, 1fr)")).join(" ");

  const headerCells = columns.map((c) => (
    <div
      key={c.key}
      className={cn(
        "flex items-center gap-1 truncate px-2 text-2xs font-medium uppercase tracking-wider text-fg-faint",
        c.align === "right" && "justify-end",
        c.sort && "cursor-pointer select-none hover:text-fg",
      )}
      onClick={c.sort ? () => setSort((s) => s?.key === c.key ? { key: c.key, dir: s.dir === 1 ? -1 : 1 } : { key: c.key, dir: -1 }) : undefined}
      role={c.sort ? "button" : undefined}
      tabIndex={c.sort ? 0 : undefined}
      onKeyDown={c.sort ? (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); setSort((s) => s?.key === c.key ? { key: c.key, dir: s.dir === 1 ? -1 : 1 } : { key: c.key, dir: -1 }); } } : undefined}
      aria-sort={sort?.key === c.key ? (sort.dir === 1 ? "ascending" : "descending") : undefined}
    >
      <span className="truncate">{c.header}</span>
      {sort?.key === c.key && <span aria-hidden className="text-accent">{sort.dir === 1 ? "↑" : "↓"}</span>}
    </div>
  ));

  const renderRow = (row: T, i: number, style?: React.CSSProperties) => {
    const key = rowKey(row, i);
    const selected = selectedKey === key;
    return (
      <div
        key={key}
        style={{ ...style, gridTemplateColumns: grid }}
        className={cn(
          "grid h-7 items-center border-b border-line/60 text-base",
          onRowClick && "cursor-pointer",
          selected ? "bg-accent-soft/70" : "hover:bg-surface-raised",
        )}
        onClick={onRowClick ? () => onRowClick(row) : undefined}
        onKeyDown={onRowClick ? (e) => { if (e.key === "Enter") onRowClick(row); } : undefined}
        tabIndex={onRowClick ? 0 : undefined}
        role={onRowClick ? "button" : undefined}
        aria-current={selected || undefined}
      >
        {columns.map((c) => (
          <div key={c.key} className={cn("truncate px-2", c.align === "right" && "text-right", c.mono && "font-mono text-sm")}>
            {c.cell(row, i)}
          </div>
        ))}
      </div>
    );
  };

  if (!rows.length) return <>{empty}</>;

  return (
    <div className={cn("flex min-h-0 flex-col", className)}>
      <div
        style={{ gridTemplateColumns: grid }}
        className={cn("grid h-6 shrink-0 items-center border-b border-line bg-surface-sunken", stickyHeader && "sticky top-0 z-10")}
      >
        {headerCells}
      </div>
      <div ref={parentRef} className="min-h-0 flex-1 overflow-auto" style={{ maxHeight: virtual ? maxHeight : undefined }}>
        {virtual ? (
          <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
            {virtualizer.getVirtualItems().map((v) =>
              renderRow(sorted[v.index], v.index, {
                position: "absolute", top: 0, left: 0, width: "100%", transform: `translateY(${v.start}px)`,
              }),
            )}
          </div>
        ) : (
          sorted.map((r, i) => renderRow(r, i))
        )}
      </div>
    </div>
  );
}
