import { Button } from "@/components/ui/button";
import { BitDemo } from "@/components/bit-demo";

/** Facts that are true of the product as specified — nothing invented for effect. */
const FACTS: Array<[React.ReactNode, string]> = [
  ["3", "engines, chosen at CREATE"],
  ["1", "process, one file, one tenant"],
  ["0", "write-ahead logs"],
];

export function Hero() {
  return (
    <section className="mx-auto max-w-site px-5 py-12 md:px-8 md:py-16 lg:px-12 lg:py-20">
      <div className="grid items-start gap-10 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.08fr)] lg:gap-16">
        <div>
          <h1 className="text-[34px] font-medium leading-[1.03] tracking-[-0.032em] text-fg md:text-[46px] lg:text-[58px]">
            A filter is an intersection.
            <br />
            A count is a popcount.
          </h1>

          <p className="mt-5 max-w-[44ch] text-lg leading-relaxed text-fg-muted">
            bigdb stores every fact as <b className="font-medium text-fg">one bit at (row, record)</b>.
            Nothing is scanned, decoded or materialised to answer a predicate — the engine ANDs machine
            words and counts the ones that stayed.
          </p>

          <div className="mt-7 flex flex-wrap gap-2.5">
            <Button variant="primary" size="lg" asChild><a href="https://bigdb.cloud/deployments">Open the console</a></Button>
            <Button variant="outline" size="lg" asChild><a href="#architecture">Read the architecture</a></Button>
          </div>

          <dl className="mt-8 flex flex-wrap gap-x-10 gap-y-4 border-t border-line pt-4">
            {FACTS.map(([n, label]) => (
              <div key={label}>
                <dt className="font-mono text-xl tabular text-fg">{n}</dt>
                <dd className="text-sm text-fg-faint">{label}</dd>
              </div>
            ))}
          </dl>

          <p className="mt-7 max-w-[46ch] text-base leading-relaxed text-fg-muted">
            And when you ask for something it does not do — a join, a{" "}
            <code>HAVING</code>, an <code>OFFSET</code> — it{" "}
            <a href="#refusals" className="text-refused underline decoration-refused/40 underline-offset-4 hover:decoration-refused">
              refuses by name
            </a>
            , at parse time, and tells you what exists instead.
          </p>
        </div>

        <BitDemo />
      </div>
    </section>
  );
}
