# Access control

Who somebody is, and what they may do, are two questions with two answers stored in two places.
That split is the whole design, and most of what follows falls out of it.

- **Who** is a line in a file on the server's disk, written by `big passwd`, read once at startup.
- **What** is a set of grants in the catalog, written by `GRANT`, read on every statement.

The only thing crossing the gap is a role **name**.

## Why the split

A route that could write the users file would let a privileged credential rewrite the credential
file over the network — turning "can drop every table" into "can lock the operator out and let
themselves back in", which is a much larger power and one nobody asked for. So credentials stay
out-of-band, on the disk, behind a CLI.

Privileges are the opposite case. They have to replicate, they have to be atomic against the
schema they describe, and they have to change without a restart. All three are free if they live
in the catalog: a `GRANT` travels through the same leader-then-fan-out a `CREATE TABLE` takes, it
commits at the same meta page flip, and a backup that has the tables has the privileges over them.

## The model

Three crates, and each holds only what it can:

| crate | holds |
|---|---|
| `big-rbac` | what a privilege *is*, the store, and the decision. No dependencies at all. |
| `big-db` | where roles are *kept*: two catalog record kinds, and the drop cascade. |
| `big-embed` | administering them: name rules, name→id resolution, and the transaction. |

`big-rbac` having no dependencies is what lets `big-sql` name a privilege without linking any
storage — which is the property that keeps `Sql::demands` next to the statements it is about,
rather than at an edge that would have to re-derive it.

### Privileges

Eight verbs. Seven are grantable in SQL; `OPERATE` guards routes rather than statements and is
route-only for now, because a refusal no statement can explain is a refusal nobody can debug.

```
SELECT  INSERT  DELETE  CREATE  DROP  ALTER  ROLES  (OPERATE)
```

`DELETE` is separate from `INSERT` because deleting records is reachable without inserting them —
the REST surface has a route for it — and a credential that may add facts is not obviously one
that may remove them.

`ROLES` is held on `*.*` or nowhere. A role that could hand out privileges inside one database
would still be handing out the power to hand them out, and that fence does not survive one hop.

`CREATE` is not grantable on a table: a table that exists is not one there is anything left to
create. It is held a level up, over the database that will hold it.

### Objects

Three levels and no fourth:

```
*.*          the server
db.*         every table in one database
db.table     one table
```

There is no column grant and no row filter. This surface answers questions over whole tables, and
a privilege finer than the answer is a fence somebody walks around by asking a slightly different
question. Put the columns nobody may read in a table of their own.

### Resolution

What a role holds on `(database, table)` is the **union** of its grants at all three levels.
Grants are additive; `REVOKE` removes a grant rather than adding a denial. That is what keeps the
answer independent of the order the levels are consulted in — there is no precedence to get wrong.

The decision itself is `Api::allows`: a handful of `BTreeMap` lookups over integers, no I/O. That
matters, because it runs once per demand per statement on the path where re-authenticating would
cost a second ~50 ms argon2 hash.

### `superuser`

Reserved by name, never stored, holds everything, cannot be created, dropped, or granted to.

It exists because privileges come from grants and a grant has to be made by somebody who already
holds the privilege to make one — so a database whose catalog is empty has nobody who can write
the first `GRANT`, and no amount of correct code inside the engine breaks that circle. A role that
is true before anything is written breaks it from outside.

This is the same shape `default` has as a database: true without a record behind it, so a reload
never has to decide whether to put it back.

## Where the check happens

Two places, both calling the same resolver:

1. **`refuse()`** in `big-http`, for the REST routes. They never reach `Cluster::run`, so a guard
   that only covered SQL would leave them open.
2. **`Cluster::run`**, for `POST /sql`. The statement's own demands, asked before any of the work
   — so a refused statement reaches no peer and takes no record ids from the leader.

What a statement demands is `Sql::demands`, next to the `Sql` variants. An edge deciding it by
matching on the AST is how the rule ends up written twice and enforced once.

`EXPLAIN` inherits what it wraps. It runs nothing, so on the letter of it an `EXPLAIN CREATE
TABLE` is harmless — but an authority decided from a *wrapper keyword* rather than from what the
statement is about is the shape that goes wrong the first time an explained statement has to read
something to say anything useful. ClickHouse decides it the same way.

## Statements

```sql
CREATE ROLE [IF NOT EXISTS] <name>
DROP   ROLE [IF EXISTS]     <name>
GRANT  <privileges> ON <object> TO   <role>
REVOKE <privileges> ON <object> FROM <role>
SHOW ROLES
SHOW GRANTS [FOR <role>]
```

An unqualified name means the request's database, the same rule `CREATE TABLE` follows: `ON
orders` is a table, `ON *` is that database, and `ON *.*` is the server and is never filled in.

`ALL` means "everything grantable at this level", not "every bit" — so `GRANT ALL ON sales.*` does
not hand out `ROLES`, and `GRANT ALL ON sales.orders` does not hand out `CREATE`.

### What is refused, and why

| statement | code | instead |
|---|---|---|
| `CREATE USER`, `ALTER USER`, `DROP USER` | `sql_no_users` | `big passwd`, on the server's disk |
| `GRANT SELECT(a, b) ON t` | `sql_acl_columns` | grant on the table |
| `GRANT analyst TO senior` | `sql_no_role_hierarchy` | grant the privileges, or give the person the other role |
| `TO PUBLIC` | `sql_no_public` | make a role and name it in the users file |
| `WITH GRANT OPTION` | `sql_no_grant_option` | `GRANT ROLES ON *.*`, or nothing |
| `GRANT ROLES ON db.*` | `sql_acl_object` | `ROLES` is server-wide |
| `CREATE ROLE superuser` | `sql_reserved_role` | it exists already |

## Operating

### The upgrade

**Every existing users file names roles no catalog has**, so on the day this lands every
credential holds nothing. That is deliberate — the alternative is a server that refuses to start
over a role somebody is one statement away from creating, which is exactly the state a locked-out
operator is in.

```sh
big passwd /etc/big/users role recovery superuser   # BEFORE restarting
# restart, then:
bigctl --user recovery sql "CREATE ROLE admin"
bigctl --user recovery sql "GRANT ALL ON *.* TO admin"
```

`crates/big-e2e/tests/e2e/rbac.rs` runs this drill start to finish against real processes.

### What is live and what is not

| change | takes effect |
|---|---|
| `GRANT`, `REVOKE`, `DROP ROLE` | next statement, cluster-wide |
| password change, `big passwd delete` | ≤ 60 s (the verification cache) |
| `big passwd role <user> <name>` | **restart** |

The last row is the trap. There is no users-file reload anywhere, so operators who see `GRANT`
apply instantly will reasonably assume role reassignment does too.

The 60-second cache holds **which line of the users file a password matched**, not what that role
may do — so a grant is never stale, and the window is about passwords only.

### ⚠️ A partial `REVOKE`

`Cluster::ddl` is leader-then-fan-out with `ClusterError::Partial`; there is no consensus here to
fix that structurally. A partial *schema* change fails loudly on the next query. A partial
*revoke* leaves the privilege live on one node in a load-balanced pool — an intermittent success
where a refusal was wanted, which is the failure mode nobody notices.

**A `REVOKE` reported as partial must be re-run until it is not.** The error names the nodes.

### Ceilings

**64 roles, 256 grants.** The catalog is re-encoded on every commit that dirties it — including
ones that only wrote data, because fragment metadata lives in the same chain and moves whenever a
bit depth widens or a shard's min/max shifts. So an import rewrites every grant record too.

If the ceiling ever binds, the fix is not a bigger number: give the store its own page chain and
its own dirty flag, which the meta page has room for and which would take data commits out of the
picture entirely. That is additive — `dec_pgno` treats `0` as absent, so an older file reads back
as having none.

## Known limits

- **A view gives no privilege indirection.** Views are expanded on the parse tree before
  `demands()` sees the statement, so `SELECT * FROM v` demands `SELECT` on the *base table* and
  `GRANT SELECT ON db.v` grants nothing usable.
- **Listings are not filtered.** `SHOW DATABASES` and `SHOW TABLES` answer with names to anybody
  who authenticates. Narrowing them to what the reader may query belongs beside `Sql::demands`
  when it happens.
- **No column or row-level control**, by decision rather than omission — see *Objects* above.
- **`OPERATE` is not grantable in SQL**, only checked at the routes.

## What this reversed

The published position was *"Roles are verbs, not rows — there are no row filters and no
per-column grants. Two audiences means two deployments."* Table-level grants **are** rows, so half
of that is now false and the sentence needs revising rather than deleting: column grants and row
filters genuinely stay out of scope, and process isolation is still the answer for a hard
multi-tenant boundary. What changed is that "one deployment, two audiences" stopped requiring two
processes for the ordinary case of a dashboard that reads one database.
