import { Band, BandHead } from "@/components/section";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/cn";

/** Metered on the three things the engine actually spends. */
const TIERS = [
  {
    name: "Self-hosted",
    price: "$0",
    per: "Apache 2.0 · all three engines · no node limit",
    items: ["bitmap, bitmap+columnar and columnar", "Cluster mode", "Community support"],
    cta: { label: "Install it", href: "#install", primary: false },
  },
  {
    name: "Cloud",
    price: "$0.09",
    per: "per GB stored, per month · queries and ingested facts metered separately",
    items: ["Managed backups, which also compact", "Console, metrics and request log", "Tokens scoped per deployment"],
    cta: { label: "Open the console", href: "/deployments", primary: true },
  },
  {
    name: "Enterprise",
    price: "Custom",
    word: true,
    per: "Single-tenant regions · contracted response times",
    items: ["Dedicated hardware per tenant", "Migration engineering", "Source escrow"],
    cta: { label: "Contact us", href: "#install", primary: false },
  },
];

export function Pricing() {
  return (
    <Band id="pricing">
      <BandHead
        title="Pricing"
        lede="The engine is identical in all three. You pay for someone else running it."
      />
      <div className="grid gap-px overflow-hidden rounded-lg border border-line bg-line lg:grid-cols-3">
        {TIERS.map((t) => (
          <div key={t.name} className="flex flex-col bg-surface p-5 pb-6">
            <h3 className="text-md font-normal text-fg-muted">{t.name}</h3>
            <div className={cn(
              "mt-3 tracking-tight text-fg tabular",
              t.word ? "font-sans text-[30px]" : "font-mono text-[34px]",
            )}>
              {t.price}
            </div>
            <p className="mt-0.5 min-h-[36px] text-sm leading-relaxed text-fg-faint">{t.per}</p>
            <ul className="my-5 space-y-1.5 text-base text-fg-muted">
              {t.items.map((i) => (
                <li key={i} className="relative pl-5">
                  <span aria-hidden className="absolute left-0 top-[7px] size-2 rounded-[1px] bg-accent" />
                  {i}
                </li>
              ))}
            </ul>
            <Button variant={t.cta.primary ? "primary" : "outline"} size="md" className="mt-auto w-full" asChild>
              <a href={t.cta.href}>{t.cta.label}</a>
            </Button>
          </div>
        ))}
      </div>
    </Band>
  );
}
