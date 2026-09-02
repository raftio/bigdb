import * as React from "react";
import { Snippet } from "@/components/snippet";
import { Button } from "@/components/ui/button";
import {
  C, Caption, DocHeader, DocPager, DocSection, DocStep, Note, P,
} from "@/components/docs/docs-prose";
import type { TocItem } from "@/components/docs/docs-toc";
import { cn } from "@/lib/cn";

/* ──────────────────────────────────────────────────────────────────────────
   The data. Every string below is something the server actually says: an
   engine name off `big_engine::ENGINES`, a kind off `parse_kind`, an error
   code off `DbError::code`. A doc that invents a spelling is a doc that
   teaches a request the daemon will refuse.
   ────────────────────────────────────────────────────────────────────── */

export const CREATE_TABLE_TOC: TocItem[] = [
  { id: "engine", label: "1. Choose the engine" },
  { id: "create", label: "2. Create it" },
  { id: "fields", label: "3. The field kinds" },
  { id: "first-fact", label: "4. Write a fact" },
  { id: "rules", label: "What it will not do" },
  { id: "next", label: "Next steps" },
];

export const CREATE_TABLE_MD = "/docs/create-table.md";

type Engine = { id: string; writes: string; good: string; cost: string; note?: string };

const ENGINES: Engine[] = [
  {
    id: "bitmap",
    writes: "Bitmaps and bit-sliced indexes",
    good: "Filters and counts, which are an intersection and a popcount rather than a scan.",
    cost: "Values are not stored. An integer rebuilds from its bit planes; a keyed or boolean column has no value to hand back, and asking for one is refused when the plan is built.",
  },
  {
    id: "bitmap+columnar",
    writes: "Both, for every fact",
    good: "Everything either half is good at — the planner reads whichever answers the question cheaper.",
    cost: "Two copies of every fact on the write path, and the disk to hold them.",
    note: "default",
  },
  {
    id: "columnar",
    writes: "Column segments, plus the existence row",
    good: "Aggregates and projections that read a column end to end.",
    cost: "No index to intersect, so a filter scans. Time windows and named views are refused by name: they live in an index this table does not keep.",
  },
];

type Line = { code: string; copy?: boolean; caption?: React.ReactNode };

const CREATE_LINES: Line[] = [
  {
    code: "CREATE TABLE events",
    caption: (
      <>
        A name and nothing else. No <C>ENGINE</C> clause means the server&apos;s default —{" "}
        <C>bitmap+columnar</C>, the engine that answers the widest range of questions well, rather
        than any particular one.
      </>
    ),
  },
  {
    code: "CREATE TABLE events ENGINE = 'bitmap+columnar'",
    caption: (
      <>
        Quoted because <C>+</C> is not a character the lexer has a token for.{" "}
        <C>ENGINE = columnar</C> is fine bare.
      </>
    ),
  },
  {
    code: "CREATE TABLE events (country TEXT, amount INT, price DECIMAL(10, 2))",
    caption: (
      <>
        A column list, which is optional. Each column becomes exactly the field{" "}
        <C>POST /table/{"{t}"}/field/{"{f}"}</C> would have created — same kind, same depth, same
        scale in <C>/schema</C> — so this is one statement to write rather than a second way to
        have a field.
      </>
    ),
  },
  {
    code: '{"columns":["table"],"rows":[[3]]}',
    copy: false,
    caption: (
      <>
        What <C>POST /sql</C> answers: the table id in a one-cell result set, so a SQL client reads
        a result set rather than a second response shape.
      </>
    ),
  },
];

type Kind = { id: string; sql: string; holds: string; opts: string };

const KINDS: Kind[] = [
  { id: "set", sql: "TEXT, VARCHAR, CHAR, STRING", holds: "Many keys per record. A second write adds.", opts: "—" },
  { id: "mutex", sql: "MUTEX", holds: "One key per record. A second write replaces the first.", opts: "—" },
  { id: "bool", sql: "BOOL, BOOLEAN", holds: "Two rows, true and false.", opts: "—" },
  { id: "int", sql: "TINYINT, SMALLINT, INT, BIGINT", holds: "An unsigned integer, bit-sliced.", opts: "bit_depth (32), or UINT(bits)" },
  { id: "signed", sql: "SIGNED, INT SIGNED, BIGINT SIGNED", holds: "A signed integer, in the same planes under a bias.", opts: "bit_depth (32), or SIGNED(bits)" },
  { id: "decimal", sql: "DECIMAL(p, s), NUMERIC(p, s)", holds: "An integer compared as a value with digits after the point.", opts: "scale (required), bit_depth" },
  { id: "float32", sql: "FLOAT, REAL, FLOAT32", holds: "Single precision, in 32 planes under an order-preserving bit transform.", opts: "—" },
  { id: "float64", sql: "DOUBLE, DOUBLE PRECISION, FLOAT64", holds: "Double precision, the same transform over 64 planes.", opts: "—" },
  { id: "date", sql: "DATE", holds: "Days since 1970-01-01, signed, written '2024-01-15'.", opts: "—" },
  { id: "datetime", sql: "DATETIME, TIMESTAMP", holds: "Seconds since 1970-01-01 UTC, written '2024-01-15 10:30:00'.", opts: "—" },
  { id: "timequantum", sql: "TIMEQUANTUM", holds: "A key, plus a bitmap view per granularity, so a window reads only the days in it.", opts: "—" },
];

type Rule = { title: string; body: React.ReactNode };

const RULES: Rule[] = [
  {
    title: "The engine is fixed at creation",
    body: (
      <>
        Creating the same table again under the same engine returns the same id. Under a different
        one it is refused — <C>table_redefined</C>, because handing back a bitmap-only table to
        somebody who wrote <C>columnar</C> is handing them a table they did not ask for. Changing an
        engine means creating a second table and copying into it.
      </>
    ),
  },
  {
    title: "A statement is not a transaction",
    body: (
      <>
        A column list and a multi-clause <C>ALTER</C> are each one statement, not one change: the
        table and every field still go to the schema leader separately, and nothing rolls back.
        What a statement buys is one round of judgement before the first change goes out — so the
        failure it can leave behind is a node that went quiet, never a <C>DECIMAL</C> somebody
        forgot to give a scale or an <C>ADD</C> that landed beside a <C>DROP</C> naming a field
        that was never there.
      </>
    ),
  },
  {
    title: "A field's kind and depth are fixed once written",
    body: (
      <>
        <C>MODIFY</C>, <C>ALTER COLUMN</C>, <C>CHANGE</C> and <C>RENAME</C> are refused by name:
        the kind is how every fact in the field was routed and the depth is how many bitmaps hold
        a value, so changing either means rewriting every fact ever written to it. That is a
        second field, a copy, and a drop — three statements, because it is three changes rather
        than one hidden inside a <C>MODIFY</C> that would look free.
      </>
    ),
  },
  {
    title: "Names are flat, and short",
    body: (
      <>
        There are no schemas or namespaces to qualify a name with — one global namespace per
        deployment. The console holds you to lowercase letters, digits and underscores; the
        server&apos;s own limit is 104 bytes, which is what is left of a 128-byte catalog entry
        after its header.
      </>
    ),
  },
  {
    title: "A decimal without a scale is refused",
    body: (
      <>
        Not defaulted to zero: a decimal with no scale is an integer wearing a different name, and{" "}
        <C>price {">"} 5</C> means <C>{">"} 500</C> on a field with two of them. The difference
        matters to every comparison written against it, so it is asked for up front.
      </>
    ),
  },
  {
    title: "A schema change is applied everywhere or reported",
    body: (
      <>
        One DDL goes to the schema leader and then to every node, so a partial success is the
        cluster&apos;s to report rather than something a client discovers later as a table that
        exists on three machines out of four.
      </>
    ),
  },
];

/* ────────────────────────────────────────────────────────────────────── */

export function CreateTableDoc() {
  return (
    <>
      <Header />
      <Engines />
      <Create />
      <Fields />
      <FirstFact />
      <Rules />
      <NextSteps />
      <DocPager
        prev={{ label: "Install bigdb", href: "/#install" }}
        next={{ label: "What bigdb refuses", href: "/#refusals" }}
      />
    </>
  );
}

function Header() {
  return (
    <DocHeader
      crumbs={[{ label: "bigdb", href: "/" }, { label: "docs" }, { label: "create a table" }]}
      tags={["Schema", "SQL", "Getting started"]}
      title="Create a table"
      lede={
        <>
          A table here is a name, a storage engine, and the fields you declare on it. The engine it
          is created under is chosen once and decides which questions it can answer cheaply — and
          which it refuses outright.
        </>
      }
    >
      <div className="mt-6 flex flex-wrap items-center gap-2.5 print:hidden">
        <Button variant="primary" size="lg" asChild>
          <a href="https://bigdb.cloud/deployments">Do it in the console</a>
        </Button>
        <Button variant="outline" size="lg" asChild>
          <a href="/#install">Run it locally first</a>
        </Button>
      </div>
      <Note title="Before you start">
        Creating schema needs an <C>admin</C> token. Writing facts afterwards needs only{" "}
        <C>write</C>, and reading needs <C>read</C>.
      </Note>
    </DocHeader>
  );
}

function Engines() {
  return (
    <DocStep
      id="engine"
      n={1}
      title="Choose the engine"
      lede="What a table writes for every fact. Chosen once, at creation, and never changed — so this is the decision to make before the name."
    >
      <div className="grid gap-3">
        {ENGINES.map((e) => (
          <div
            key={e.id}
            className={cn(
              "rounded border p-4",
              e.note ? "border-accent-line bg-accent-soft/35" : "border-line bg-surface",
            )}
          >
            <div className="mb-3 flex items-baseline gap-2">
              <code className="font-mono text-md font-medium text-fg">{e.id}</code>
              {e.note && (
                <span className="font-mono text-2xs uppercase tracking-wide text-accent">{e.note}</span>
              )}
            </div>
            <dl className="grid gap-2.5 text-base leading-relaxed sm:grid-cols-[92px_minmax(0,1fr)] sm:gap-x-5">
              <Pair k="Writes" v={e.writes} />
              <Pair k="Answers well" v={e.good} />
              <Pair k="Costs" v={e.cost} />
            </dl>
          </div>
        ))}
      </div>
      <Note title="Under every engine">
        The existence row is kept under columnar too: it is one bit per record, and it is what{" "}
        <C>NOT</C>, <C>count(*)</C> and the record cursor stand on.
      </Note>
    </DocStep>
  );
}

function Pair({ k, v }: { k: string; v: string }) {
  return (
    <>
      <dt className="font-mono text-2xs uppercase tracking-wide text-fg-faint sm:pt-1">{k}</dt>
      <dd className="text-fg-muted">{v}</dd>
    </>
  );
}

function Create() {
  return (
    <DocStep
      id="create"
      n={2}
      title="Create it"
      lede="One statement, with or without a column list. The list is optional because a table with no fields is a real thing here, and because there is no ALTER: adding a field to a table that already exists is still the field route's job."
    >
      {CREATE_LINES.map((l, i) => (
        <div key={i}>
          <Snippet code={l.code} copy={l.copy} />
          {l.caption && <Caption>{l.caption}</Caption>}
        </div>
      ))}

      <Note title="Refused" tone="refused">
        <code className="mb-1.5 block font-mono text-sm text-fg">
          CREATE TABLE events (amount INT(11), price DECIMAL(10), country TEXT NOT NULL)
        </code>
        Three refusals, each by name. <C>INT(11)</C> is a display width where it was written and
        would be eleven <i>bits</i> here, which stops at 2047 — so it is refused rather than
        reinterpreted. <C>DECIMAL(10)</C> is SQL for ten digits and no fraction, and reading it as
        a scale would make <C>price {">"} 5</C> a question about ten-billionths. <C>NOT NULL</C> is
        a promise about rows, and a fact at <C>(field, record)</C> is not a row.
      </Note>
      <P>
        Nothing is created until the whole list has been read: every kind, depth and scale is
        decided while the statement is parsed, so a list with a bad type in it leaves no table
        behind. It is still not a transaction — the table and each field are separate changes
        below this line — so what can fail part-way is a node going quiet, reported as a partial
        schema change naming what did land.
      </P>

      <P>
        The statement is classified before anything is planned, because the two kinds go to
        different places: a query is planned where it arrives, a schema change goes to the leader
        and then to every node. That is what keeps a <C>CREATE TABLE</C> from being applied on
        whichever machine the client happened to reach.
      </P>
      <P>
        <C>POST /sql</C> is authorised as <C>read</C>, and this statement raises that to <C>admin</C>{" "}
        once the parser has classified it — a read-only token cannot create a table by writing it in
        SQL.
      </P>
      <P>
        Everything else is still refused by name: <C>INSERT</C>, <C>UPDATE</C>, <C>DELETE</C>,{" "}
        <C>DROP TABLE</C>, and every other <C>CREATE</C>. This surface writes no rows, and the
        schema it changes is a table&apos;s and its fields&apos;.
      </P>

      <h3 className="mt-8 mb-3 text-md font-medium text-fg">Changing it afterwards</h3>
      <P>
        <C>ALTER TABLE</C> adds and drops fields — the two changes the engine below can make to
        one.
      </P>
      <Snippet code="ALTER TABLE events ADD COLUMN region TEXT, DROP COLUMN legacy" />
      <Snippet code={'{"columns":["fields"],"rows":[[2]]}'} copy={false} />
      <Caption>
        How many fields changed, not an id: two changes have no single id to answer with.{" "}
        <C>COLUMN</C> is optional, the clauses run in the order written, and every one of them is
        judged before the first is applied — so <C>ADD b, DROP nope</C> creates nothing.
      </Caption>
      <Note title="Refused" tone="refused">
        <code className="mb-1.5 block font-mono text-sm text-fg">
          ALTER TABLE events MODIFY amount BIGINT
        </code>
        There is no third clause. A field&apos;s kind is how every fact in it was routed and its
        bit depth is how many bitmaps hold a value, so neither can change without rewriting every
        fact ever written to it — <C>MODIFY</C>, <C>ALTER COLUMN</C> and <C>CHANGE</C> are refused
        with that sentence. <C>RENAME</C> is refused because names are what resolve a fact all the
        way down and nothing below renames one, and <C>ENGINE =</C> because the facts already there
        were written under the old engine. Each means a second field or table, the facts copied
        into it, and the first dropped — three statements, because it is three changes.
      </Note>
    </DocStep>
  );
}

function Fields() {
  return (
    <DocStep
      id="fields"
      n={3}
      title="The field kinds"
      lede="A kind decides how a fact is routed and what a second write to the same record means. Every column in the list above is one of these, and so is every field the route creates."
    >
      <P>
        Two spellings, one field. A column list names a kind the way SQL names a type; the route
        names it directly, and it is the route you reach for when the table already exists.
      </P>
      <Snippet
        code={"POST /table/events/field/country?kind=set\nPOST /table/events/field/amount?kind=int&bit_depth=32\nPOST /table/events/field/price?kind=decimal&scale=2"}
        copy={false}
        className="mb-6"
      />
      <div className="overflow-x-auto">
        <table className="w-full min-w-[760px] border-collapse text-base">
          <thead>
            <tr className="border-b border-line-strong text-left">
              <Th>kind</Th>
              <Th>written in SQL as</Th>
              <Th>what a record holds</Th>
              <Th>parameters</Th>
            </tr>
          </thead>
          <tbody>
            {KINDS.map((k) => (
              <tr key={k.id} className="border-b border-line align-top">
                <td className="py-2.5 pr-5"><code className="font-mono text-fg">{k.id}</code></td>
                <td className="py-2.5 pr-5 font-mono text-sm text-fg-faint">{k.sql}</td>
                <td className="py-2.5 pr-5 leading-relaxed text-fg-muted">{k.holds}</td>
                <td className="py-2.5 font-mono text-sm text-fg-faint">{k.opts}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <P className="mt-5">
        <C>bit_depth</C> is how many bits a value may occupy, and every plane is one more bitmap to
        intersect during a range query — a wider field is a slower one, so declare what the data
        needs rather than the maximum. A value wider than the depth is refused on write, never
        wrapped.
      </P>
      <P>
        The SQL integer names carry the widths they have always carried: <C>SMALLINT</C> is 16 bits
        and <C>BIGINT</C> is 64. <C>UINT(bits)</C> and <C>SIGNED(bits)</C> exist for the depths no
        SQL name has, which is most of the useful ones. A decimal&apos;s precision buys its depth —{" "}
        <C>DECIMAL(10, 2)</C> is 34 bits, enough to hold every ten-digit number and no more.
      </P>
    </DocStep>
  );
}

function Th({ children }: { children: React.ReactNode }) {
  return (
    <th className="py-2 pr-5 font-mono text-2xs font-normal uppercase tracking-wide text-fg-faint">
      {children}
    </th>
  );
}

function FirstFact() {
  return (
    <DocStep
      id="first-fact"
      n={4}
      title="Write a fact, then count it"
      lede="Writing is the other thing this surface does not do. Facts go in over the import route — one per line, field, record, value — and SQL is how you check they landed."
    >
      <Snippet code={"POST /table/events/import"} copy={false} />
      <Snippet code={"country 1 vn\namount 1 4200"} copy={false} />
      <Caption>
        How the value is read is the field&apos;s kind to decide: a number for an integer, <C>true</C>{" "}
        or <C>false</C> for a boolean, <C>key@seconds</C> for a time quantum, a string for the rest.
      </Caption>

      <Snippet code={"SELECT count(*) FROM events WHERE country = 'vn'"} />
      <Snippet code={'{"columns":["count"],"rows":[[1]]}'} copy={false} />
      <Caption>
        The column is named <C>count</C> because that is what the select list asked for. A filter is
        an intersection and the count is a popcount over it — no rows are read to answer this.
      </Caption>

      <P>
        The whole batch is resolved against the schema before any of it is written, so a batch that
        turns out to be malformed does not land halfway.
      </P>
      <P>
        <C>INSERT</C> is refused by name rather than translated into this route: an insert promises
        a row, and what lands here is a fact at <C>(field, record)</C>. A body goes in one request
        the server bounds at 8 MiB; a whole file is <C>bigi</C>&apos;s job.
      </P>
      <P>
        To see the table itself rather than its records, <C>GET /schema</C> answers with every
        table, its engine and its fields — which is also what the console&apos;s workbench
        autocompletes from.
      </P>
    </DocStep>
  );
}

function Rules() {
  return (
    <DocSection
      id="rules"
      title="What creating a table will not do"
      lede="Each of these is a refusal with a name and a reason, raised where you can still act on it rather than discovered later as a wrong answer."
    >
      <ul className="grid gap-5">
        {RULES.map((r) => (
          <li key={r.title} className="border-l-2 border-refused/45 pl-4">
            <h3 className="mb-1.5 text-md font-medium text-fg">{r.title}</h3>
            <p className="max-w-[68ch] text-base leading-relaxed text-fg-muted">{r.body}</p>
          </li>
        ))}
      </ul>
    </DocSection>
  );
}

function NextSteps() {
  return (
    <DocSection id="next" title="The table is empty. Fill it.">
      <div className="flex flex-wrap gap-2.5 print:hidden">
        <Button variant="primary" size="lg" asChild>
          <a href="https://bigdb.cloud/deployments">Open the console</a>
        </Button>
        <Button variant="outline" size="lg" asChild>
          <a href="/#refusals">See what is refused</a>
        </Button>
      </div>
    </DocSection>
  );
}
