# bigdb Cloud — console

The hosted control plane for `big`, a bitmap-native analytical database.

```bash
cd console
npm install
npm run dev     # http://localhost:4000
```

The marketing site is a separate app in `landing/` (Vite + React). It shares
this project's design tokens and copies the handful of components it needs, so
the two look like one product without the console carrying marketing routes.

Runs entirely against a fixture layer — no `bigd` required. Point it at a real
server by setting `NEXT_PUBLIC_BIGDB_API`; the client module is the only file
that changes.

---

## Design rationale

### What this is optimising for

An engineer opens a database console with a question already in mind and a
terminal one keystroke away. The console has to be *faster than the terminal* at
the two things a terminal is bad at — seeing a shape, and seeing many numbers at
once — and must never be slower at the thing a terminal is good at, which is
running one query and reading the answer.

So: **numbers are the hero and chrome is quiet.** Rows are 28px. Every number,
id, token, query and error code is mono and tabular-aligned, so a column of
figures reads as a column. Semantic colour is reserved for state — healthy,
degraded, refused, failed — and never used decoratively, which is what keeps it
legible when it does appear.

**The accent means exactly one thing: a bit that is set.** It is periwinkle in
dark, deep blue in light, and it has an opposite number — `--unset`, the colour
of a bit that is not. The mark is two of each. That pair is the whole visual
thesis, and nothing decorative is allowed to spend it: the landing page's bit
grid, the engine mix bar, the primary button and the focus ring are all the same
claim. Light is warm paper (`#F6F6F3`) with cool ink; dark is near-black and
neutral. Type is Archivo over JetBrains Mono, radii are 2/3/5px — the product
draws boxes, not pills.

Charts are small multiples and sparklines, not posters. Latency is always a
p50 line inside a p95–p99 band read off `big_http_request_duration_seconds`;
there is no mean anywhere in the product, because a mean over a heavy tail
describes a request nobody made.

### How refusal-first shows up

`big` refuses unsupported constructs **by name, at parse time, with a stable
code and a sentence saying what exists instead.** That is not an error path —
it is the clearest statement the product makes about what it is. So the console
treats a refusal as a designed, first-class state, everywhere it can appear:

**It has its own hue.** Refusals are magenta — far enough from the accent that a
refused row and a set bit are never confused. Failures are red. They are different
events — one produced no answer because something broke, the other produced no
answer because the question does not exist here — and the console never lets them
share a colour, a badge, or a sentence. This holds in the workbench, in the
request log, in the overview's response breakdown, in query history, and in
ingest's rejected lines.

**It names the construct, not the failure.** The panel headline is
`Refused · JOIN`, not "Error". The stable code (`sql_no_joins`) sits beside it as
a badge you can grep your logs for, with the HTTP status next to that.

**It points at the text.** The server returns the byte span it refused. The
editor underlines exactly that span in magenta — the same magenta — and the panel
repeats it in context with the surrounding characters. You are never left
counting bytes.

**It says what exists instead, and where possible does it for you.** Every
catalogue entry carries one sentence of reason and one of alternative. Where the
alternative is mechanical, there is a one-click rewrite: `JOIN` strips the join
*and* the now-pointless table aliases; `SELECT country FROM events` becomes
`GROUP BY country ORDER BY count(*) DESC`; `HAVING` and `OFFSET` are excised.

**It does not destroy your place.** A refusal leaves the previous answer on
screen, labelled "showing the previous answer", because that answer is still
true. Nothing ran, and nothing changed.

**The editor refuses before the request does.** The SQL grammar marks `JOIN`,
`HAVING`, `OFFSET`, `UNION`, `OVER`, `NULL` and the write verbs as *invalid*
tokens, so they carry the refusal underline as you type them. Autocomplete is
driven entirely by `GET /schema` and the real call vocabulary, so the console
will not suggest something the server would reject.

The same shape covers ingest: a fact line is validated against the schema in the
browser before a byte is sent, and each rejected line gets a code, the line as
written, and the reason — the same contract as a refused query.

### Other decisions worth stating

**Engine travels with the table.** `bitmap | bitmap+columnar | columnar` is fixed
at CREATE and decides what a query costs, so the badge appears wherever a table
is named — schema, workbench toolbar, command palette, ingest, deployments list
(as a mix bar). It is drawn, not typed, because the box characters it wants are
not in every mono face.

**Roles are shown, not hidden.** A `read` token sees every admin control,
disabled, with a tooltip naming the role it needs and the role it has. Hiding
them would teach the wrong shape of the product; disabling them teaches the right
one and says which token to go get. Seats (console) and token roles (database)
are explicitly distinguished on the Team page, because conflating them is the
easy mistake.

**Blast radius before confirmation.** Every destructive or cluster-wide action
states what it touches in counts — fields, fragments, records, replicas — before
it asks, and irreversible ones require typing the object's own name.

**The architecture is a live diagram.** The overview renders the request path
(edge → cluster → facade → planner → data → storage) with each layer showing its
own metric, because that stack *is* the mental model, and reading it top to
bottom tells you where the time and the bytes went.

**Nothing in the UI outruns the API.** There is no feature here that
`bigd`'s routes cannot serve. Restore has no button, because there is no restore
route — it is a file swap with the process stopped, so the screen gives you the
commands instead. `/admin/backup` is per node, and the backups screen says in
plain words that a cluster backup is not one snapshot.

---

## Structure

```
src/
  app/                  routes; no page file over ~300 lines
  components/
    ui/                 primitives: button, input, dialog, tabs, select, …
    badges/             engine, role, health, status — the domain vocabulary
    data/               table, sparkline, latency band, diff table, metric tile,
                        refusal panel, states, confirm dialog, role guard
    editor/             CodeMirror 6: two grammars, schema-driven completion
    workbench/          result shapes, timing strip, history
    schema/             field-kind badge, create dialogs
    shell/              app shell, command palette, theme, connection banner
  lib/
    api/                typed client (one method per route) + mock/fixtures
    refusals.ts         the refusal catalogue: code → reason → alternative
    hooks/              deployment, schema, metrics, history, workbench
```

`src/app/globals.css` holds the tokens for both themes. `src/lib/refusals.ts` is
the file to read first — it is the product thesis in one module.

## Keyboard

`⌘K` command palette · `/` search · `g` then `v s q i c b a o` to navigate ·
`⌘↵` run query · `esc` dismiss. Every control is reachable by tab with a visible
focus ring, and `prefers-reduced-motion` disables all transitions.
