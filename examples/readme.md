# examples

Things you can run, rather than read. Each directory is a `docker compose` stack and a `run.sh`
that needs no arguments, so trying one costs a command and a minute.

| | What it runs | What it is for |
|---|---|---|
| [`producer/`](producer/) | bigdb, and an app writing into it | [`contrib/big-message`](../contrib/big-message/) — a stream of messages becoming rows |
| [`golang-ex/`](golang-ex/) | bigdb, and the same app twice | [`clients/go`](../clients/go/) — a writer that names its own record ids, so the second run changes nothing |

`redis-sink/` belongs in that table and is not there yet: it waits on
`contrib/big-message-redis`, which is half written.

**These are demos, not deployments.** They put credentials in compose files and data in
throwaway volumes because that is what makes them one command. [`deploy/`](../deploy/readme.md)
is the directory to copy from when the thing has to stay up.
