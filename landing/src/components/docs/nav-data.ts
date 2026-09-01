import {
  BarChart3, Boxes, Rocket, Table2, Terminal, type LucideIcon,
} from "lucide-react";

/**
 * The docs tree. Every `href` here points at something that exists today — a page, or a
 * band on the home page. A topic that is planned but unwritten is marked `soon` and is not
 * a link: a sidebar that promises a page and then 404s is worse than one that says "later".
 */
export type NavItem = {
  label: string;
  href?: string;
  /** An id on the current page. Highlights with the reader's scroll position. */
  anchor?: string;
  soon?: boolean;
  items?: NavItem[];
};

export type NavGroup = { label: string; icon: LucideIcon; items: NavItem[] };

export const DOCS_NAV: NavGroup[] = [
  {
    label: "Get started",
    icon: Rocket,
    items: [
      { label: "Overview", href: "/" },
      { label: "What bigdb refuses", href: "/#refusals" },
      { label: "Architecture", href: "/#architecture" },
    ],
  },
  {
    label: "Setup",
    icon: Terminal,
    items: [
      { label: "Install", href: "/#install" },
      { label: "Console", href: "https://bigdb.cloud/deployments" },
      { label: "bigc, bigi and the HTTP API", soon: true },
    ],
  },
  {
    label: "Schema",
    icon: Table2,
    items: [
      {
        label: "Create a table",
        href: "/docs/create-table/",
        items: [
          { label: "Choose the engine", anchor: "engine" },
          { label: "Create it", anchor: "create" },
          { label: "Declare its fields", anchor: "fields" },
          { label: "Write a fact", anchor: "first-fact" },
          { label: "What it will not do", anchor: "rules" },
        ],
      },
      { label: "Field kinds in depth", soon: true },
      { label: "Importing facts", soon: true },
    ],
  },
  {
    label: "Query",
    icon: Boxes,
    items: [
      { label: "The SQL surface", soon: true },
      { label: "Refusals by name", href: "/#refusals" },
      { label: "Time quanta and windows", soon: true },
    ],
  },
  {
    label: "Operate",
    icon: BarChart3,
    items: [
      { label: "Benchmarks", href: "/#benchmark" },
      { label: "Pricing and limits", href: "/#pricing" },
      { label: "Cluster and replication", soon: true },
    ],
  },
];

/** `/docs/create-table/index.html` and `/docs/create-table` are the same page to a reader. */
export function normalizePath(p: string) {
  return p.replace(/index\.html$/, "").replace(/\/+$/, "") || "/";
}
