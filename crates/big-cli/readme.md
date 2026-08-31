# big-cli

`bigc` — ask a running `bigd` a question.

**This crate links nothing.** Not `big-api`, not `big-plan`, not `big-sql`. That empty
`[dependencies]` section is the design, not an economy: a client that cannot link the engine
cannot grow an offline query path and cannot validate a statement before sending it. Both would
be a second surface, and a second surface drifts from the first.

So a statement travels as bytes and a refusal comes back as the server's own code and the
server's own sentence, printed without rewording. `sql_no_joins` on a terminal *is* the string
`bigd` chose.

```sh
bigc sql "SELECT country, count(*) FROM tx GROUP BY country"
bigc query tx 'Count(Row(country="GB"))'
bigc records tx --limit 1000
bigc shell
```

Every subcommand is exactly one public route — there is no command that loops, pages, or
composes two of them, which means a feature request for the client is a feature request for the
server. Output is aligned columns to a terminal and TSV to a pipe; `--format json` hands over the
body untouched. Exit codes: `0` answered, `1` refused, `2` usage, `3` nothing listening.

An offline mode, a `--token` flag, line editing and a config file are each refused with a
reason written down, and `bigc --help` is the surface itself.
