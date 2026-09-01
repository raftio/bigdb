import { Band, BandHead } from "@/components/section";
import { Snippet } from "@/components/snippet";

/** Every command here maps to a route that exists. Nothing else is offered. */
const COMMANDS = [
  "curl -fsSL https://bigdb.cloud/install | sh",
  "bigd --file ./events.big --listen :8080 --tokens ./tokens",
  `curl -H "Authorization: Bearer $W" --data-binary @facts.txt \\\n  localhost:8080/table/events/import`,
];

const FACTS: Array<[string, string]> = [
  ["Processes per deployment", "1"],
  ["Files on disk", "1"],
  ["Write-ahead logs", "0"],
  ["Runtime dependencies", "none"],
  ["TLS", "at the proxy"],
];

export function Install() {
  return (
    <Band id="install">
      <BandHead
        title="One binary, one file"
        lede="No agent, no coordinator, no object store to configure first. bigd is a single process over a single path you can copy."
      />
      <div className="grid gap-8 lg:grid-cols-[minmax(0,1.05fr)_minmax(0,1fr)] lg:gap-14">
        <div className="min-w-0">
          {COMMANDS.map((c) => <Snippet key={c} code={c} prompt="$ " />)}
          <p className="mt-4 max-w-[54ch] text-base leading-relaxed text-fg-muted">
            Backing it up is <code>POST /admin/backup</code>, which copies the live pages into a fresh
            file — so it compacts and migrates the format in the same pass. Restoring is putting that
            file back with the process stopped: there is no WAL to replay, because the meta page in the
            file <i>is</i> the state.
          </p>
        </div>

        <div>
          <ul className="border-t border-line">
            {FACTS.map(([k, v]) => (
              <li key={k} className="flex items-center justify-between gap-5 border-b border-line py-2.5 text-base text-fg-muted">
                <span>{k}</span>
                <b className="font-mono font-normal text-fg">{v}</b>
              </li>
            ))}
          </ul>
          <p className="mt-5 text-base leading-relaxed text-fg-muted">
            From here: <a href="https://bigdb.cloud/deployments" className="text-accent hover:underline">open the console</a>,
            <a href="/docs/create-table/" className="text-accent hover:underline"> create a table and pick its engine</a>,
            then write your first fact. The workbench autocompletes
            from <code>GET /schema</code>, so it will only ever suggest something the server can answer.
          </p>
        </div>
      </div>
    </Band>
  );
}

