import { Mark } from "@/components/section";
import { ThemeToggle } from "@/components/theme-toggle";
import { Button } from "@/components/ui/button";

/** Every entry points at a section that exists on the page. */
const NAV = [
  { href: "#refusals", label: "Refusals" },
  { href: "#benchmark", label: "Benchmarks", small: true },
  { href: "#architecture", label: "Architecture", small: true },
  { href: "#install", label: "Install", small: true },
  { href: "#pricing", label: "Pricing" },
];

export function SiteHeader() {
  return (
    <header className="sticky top-0 z-30 border-b border-line bg-bg/88 backdrop-blur">
      <div className="mx-auto flex h-14 max-w-site items-center gap-5 px-5 md:px-8 lg:px-12">
        <a href="/" className="mr-auto flex items-center gap-2.5 font-mono text-lg font-bold tracking-tighter text-fg">
          <Mark />
          bigdb
        </a>
        {NAV.map((n) => (
          <a key={n.href} href={n.href}
            className={`text-base text-fg-muted transition-colors duration-fast hover:text-fg ${n.small ? "hidden md:inline" : ""}`}>
            {n.label}
          </a>
        ))}
        <ThemeToggle />
        <Button variant="outline" asChild><a href="https://bigdb.cloud/deployments">Console</a></Button>
      </div>
    </header>
  );
}

export function SiteFooter() {
  return (
    <footer className="border-t border-line py-7 pb-10">
      <div className="mx-auto flex max-w-site flex-wrap items-center gap-x-6 gap-y-3 px-5 text-sm text-fg-faint md:px-8 lg:px-12">
        <span className="flex items-center gap-2 font-mono font-bold tracking-tighter text-fg-muted">
          <Mark size={13} /> bigdb
        </span>
        <a href="#architecture" className="hover:text-fg">Architecture</a>
        <a href="#refusals" className="hover:text-fg">Refusals</a>
        <a href="#benchmark" className="hover:text-fg">Benchmarks</a>
        <a href="#pricing" className="hover:text-fg">Pricing</a>
        <a href="https://bigdb.cloud/deployments" className="hover:text-fg">Console</a>
        <span className="ml-auto font-mono text-xs">Apache 2.0 · bigdb.cloud</span>
      </div>
    </footer>
  );
}
