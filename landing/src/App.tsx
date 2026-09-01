import { TooltipProvider } from "@/components/ui/tooltip";
import { SiteHeader, SiteFooter } from "@/components/site-chrome";
import { Hero } from "@/components/hero";
import { RefusalsSection } from "@/components/refusals-section";
import { Benchmark } from "@/components/benchmark";
import { Architecture } from "@/components/architecture";
import { Decisions } from "@/components/decisions";
import { Install } from "@/components/install";
import { Pricing } from "@/components/pricing";
import { Button } from "@/components/ui/button";

const CONSOLE_URL = "https://bigdb.cloud/deployments";

export function App() {
  return (
    <TooltipProvider delayDuration={200} skipDelayDuration={300}>
      <div className="min-h-screen">
        <SiteHeader />
        <main id="main">
          <Hero />
          <RefusalsSection />
          <Benchmark />
          <Architecture />
          <Decisions />
          <Install />
          <Pricing />

          <section className="border-t border-line py-16 md:py-20">
            <div className="mx-auto flex max-w-site flex-wrap items-end justify-between gap-7 px-5 md:px-8 lg:px-12">
              <h2 className="max-w-[16ch] text-2xl font-medium tracking-tight text-fg md:text-[40px] md:leading-[1.1]">
                Count a billion records in a millisecond.
              </h2>
              <div className="flex flex-wrap gap-2.5">
                <Button variant="primary" size="lg" asChild><a href={CONSOLE_URL}>Open the console</a></Button>
                <Button variant="outline" size="lg" asChild><a href="#install">Install locally</a></Button>
              </div>
            </div>
          </section>
        </main>
        <SiteFooter />
      </div>
    </TooltipProvider>
  );
}
