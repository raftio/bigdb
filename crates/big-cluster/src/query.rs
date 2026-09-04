// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Reads: plan once here, run the plan everywhere, merge what comes back.
//!
//! Planning is pure, so a query that will not type-check is refused before the network is
//! touched. What each owner is asked is not always what the client asked - see
//! `asked_of_owners` - and the rewrite is one case, deliberately.

use super::*;
use big_embed::{Format, GroupAt, ResultSet};

/// One engine error, as the cluster reports one of its own node's.
///
/// A free function rather than a method: a schema change refused for what it names is refused
/// at the coordinator, before any node is asked, so there is no node for it to be about.
fn local(e: big_db::DbError) -> ClusterError {
    ClusterError::Local(e.into())
}

impl<P: PagerMut + Sync> Cluster<P> {
    /// Plans once, here, and then runs the plan everywhere.
    ///
    /// Planning is pure - `big-plan` links no pager - so a query that will not type-check is
    /// refused before any node hears about it, and the network is never touched on behalf of a
    /// request that was never going to work.
    pub fn query(&self, table: &str, text: &str, opts: &QueryOptions) -> Result<Value> {
        let plan = self.api.plan(table, text)?;
        self.execute(&plan, opts)
    }

    /// What this text is, resolved against the request but run nowhere.
    ///
    /// The edge needs the value before it can decide what the request costs: the route's guard is
    /// a floor, and which privileges a statement actually needs - on which objects - is
    /// [`big_embed::Sql::demands`]'s to say, next to the variants it is about. Exposed here rather
    /// than having the edge parse for itself, so that what decides the privilege and what decides
    /// the action are one definition.
    ///
    /// **`opts` rather than nothing, and the statement rather than a verdict.** The edge used to
    /// classify against the default database and then hand the *text* back for [`Cluster::sql`]
    /// to translate a second time, against the request's database. Two translations of one
    /// statement is two answers waiting to differ, and the one that decided the role was not the
    /// one that ran. Now there is one translation: this returns the `Sql` the edge authorises,
    /// and [`Cluster::run`] takes that same value. A statement cannot be authorised as one thing
    /// and executed as another because there is only one of it.
    pub fn classify(&self, text: &str, opts: &QueryOptions) -> Result<big_embed::Sql> {
        Ok(self.api.translate_in(text, opts.database())?)
    }

    /// A schema change written in SQL, applied the way every schema change is.
    ///
    /// Answers with one number in a one-cell row, so that a client reading `/sql` gets a result
    /// set rather than a second response shape to learn.
    ///
    /// Both statements become the changes the `/table` routes already make - one `CreateTable`,
    /// one field change per column - so no node learns a second way to be told about a field
    /// and the cluster wire format gains nothing. **Neither is a transaction.** What makes that
    /// bearable is that everything decidable has been decided before the first change goes out:
    /// the kinds, depths and scales were settled while the statement was parsed, and the engine
    /// name and the fields a statement names are resolved here before anything is applied. What
    /// is left to fail mid-way is a node going quiet, which is reported as a partial schema
    /// change naming what did land - the same report a field created on its own gets.
    fn sql_ddl(&self, ddl: &big_embed::SqlDdl) -> Result<(ResultSet, Format)> {
        let (column, changed) = match ddl {
            big_embed::SqlDdl::CreateDatabase { name, if_not_exists } => {
                ("database", self.sql_create_database(name, *if_not_exists)?)
            }
            big_embed::SqlDdl::DropDatabase { name, if_exists, cascade } => {
                ("dropped", self.sql_drop_database(name, *if_exists, *cascade)?)
            }
            big_embed::SqlDdl::CreateTable { database, table, engine, columns, if_not_exists } => (
                "table",
                self.sql_create_table(
                    &qualified(database, table),
                    engine.as_deref(),
                    columns,
                    *if_not_exists,
                )?,
            ),
            big_embed::SqlDdl::AlterTable { database, table, changes } => {
                ("fields", self.sql_alter_table(&qualified(database, table), changes)?)
            }
            big_embed::SqlDdl::DropTable { database, table, if_exists } => {
                ("dropped", self.sql_drop_table(&qualified(database, table), *if_exists)?)
            }
            big_embed::SqlDdl::CreateView { database, name, body, or_replace, if_not_exists } => (
                "view",
                self.create_view(&qualified(database, name), body, *or_replace, *if_not_exists)?
                    .into(),
            ),
            big_embed::SqlDdl::DropView { database, name, if_exists } => {
                ("dropped", self.sql_drop_view(&qualified(database, name), *if_exists)?)
            }
        };
        Ok((big_embed::one_cell(column, big_embed::Datum::Int(changed.into())), Format::default()))
    }

    /// `CREATE TABLE`, answering with the table's id.
    ///
    /// The id is the leader's, which is the same number `POST /table/{t}` answers with and for
    /// the same reason.
    ///
    /// `IF NOT EXISTS` is answered here and answered *first*, before the engine name is resolved
    /// and before any column is created. The table itself is already idempotent below - an
    /// identical declaration interns to the same id - but its fields are not, so a second run of
    /// a setup script would otherwise fail on the column list rather than on the table. A table
    /// that was already there answers with nothing created, because the id of a table this
    /// statement did not create is not this statement's answer.
    fn sql_create_table(
        &self,
        table: &str,
        engine: Option<&str>,
        columns: &[big_embed::SqlColumn],
        if_not_exists: bool,
    ) -> Result<u64> {
        if if_not_exists && self.schema().iter().any(|t| t.name == table) {
            return Ok(0);
        }
        // The engine list lives in one place, and it is not `big-sql`: that crate links no
        // storage and takes the name as written, so this is where a name nobody has is refused.
        let engine = match engine {
            None => big_embed::TableEngine::default(),
            Some(name) => big_embed::TableEngine::parse(name).ok_or_else(|| {
                ClusterError::Local(big_db::DbError::UnknownEngineName(name.to_string()).into())
            })?,
        };
        let id = self.create_table_with(table, engine)?;
        for column in columns {
            self.create_column(table, column)?;
        }
        Ok(id)
    }

    /// `DROP TABLE`, answering with how many tables went - which is one, or none.
    ///
    /// Looked up here before anything is sent, so that a table nobody has is refused by this
    /// node rather than by every node in turn. `IF EXISTS` makes an absent table a request that
    /// was already satisfied, which is what makes a teardown script re-runnable.
    fn sql_drop_table(&self, table: &str, if_exists: bool) -> Result<u64> {
        if !self.schema().iter().any(|t| t.name == table) {
            if if_exists {
                return Ok(0);
            }
            return Err(local(big_db::DbError::UnknownTable(table.to_string())));
        }
        self.drop_table(table).map(u64::from)
    }

    /// `DROP VIEW`, answering with how many it dropped.
    ///
    /// `IF EXISTS` is answered here and answered first, the way `DROP TABLE`'s is: a view that
    /// is not there is a request already satisfied.
    fn sql_drop_view(&self, view: &str, if_exists: bool) -> Result<u64> {
        if !self.api.views().iter().any(|v| v.qualified() == view) {
            if if_exists {
                return Ok(0);
            }
            return Err(local(big_db::DbError::UnknownView(view.to_string())));
        }
        self.drop_view(view).map(u64::from)
    }

    /// `CREATE DATABASE`, answering with how many it created - one, or nothing when it was
    /// already there.
    ///
    /// Without `IF NOT EXISTS` a database that already exists is an error rather than a quiet
    /// zero, which is the rule `CREATE TABLE` follows one level down.
    fn sql_create_database(&self, name: &str, if_not_exists: bool) -> Result<u64> {
        if self.api.databases_named(name) {
            if if_not_exists {
                return Ok(0);
            }
            return Err(local(big_db::DbError::NameTaken(name.to_string())));
        }
        self.create_database(name).map(u64::from)
    }

    /// `DROP DATABASE`, answering with how many it dropped.
    ///
    /// `IF EXISTS` is answered here, before the drop is attempted, because a database that is
    /// not there is a request already satisfied rather than one to report on. Everything else -
    /// the default database, and the emptiness a bare `DROP` refuses on - belongs to
    /// [`Cluster::drop_database_if_empty`], which is where the `/database/{d}` route asks the
    /// same questions.
    fn sql_drop_database(&self, name: &str, if_exists: bool, cascade: bool) -> Result<u64> {
        if if_exists && !self.api.databases_named(name) {
            return Ok(0);
        }
        match self.drop_database_if_empty(name, cascade)? {
            true => Ok(1),
            false => Err(local(big_db::DbError::UnknownDatabase(name.to_string()))),
        }
    }

    /// `ALTER TABLE`, answering with how many fields it changed.
    ///
    /// A count rather than an id: a statement may add three fields and drop one, and no single
    /// id is the answer to that. It is the number of changes the statement asked for, which is
    /// the number it made - a change that did not happen is an error, never a smaller number.
    ///
    /// **Every clause is judged before the first one is applied.** The kinds and depths were
    /// settled when the statement was parsed; what is left is whether the table is there, and
    /// whether each field named is or is not, and that is checked here against the schema
    /// before anything is created or dropped. Otherwise `ADD a, DROP nope` would create `a` and
    /// then fail, which is the half-applied statement a single round of checking avoids.
    fn sql_alter_table(&self, table: &str, changes: &[big_embed::SqlAlter]) -> Result<u64> {
        let info = self
            .schema()
            .into_iter()
            .find(|t| t.name == table)
            .ok_or_else(|| local(big_db::DbError::UnknownTable(table.to_string())))?;

        // What the schema will look like as the statement runs, not as it looks now: `DROP a,
        // ADD a` is two legal changes in that order and two illegal ones in the other, and a
        // check against the schema as it stands would get both wrong.
        let mut fields: std::collections::BTreeSet<String> =
            info.fields.iter().map(|f| f.name.clone()).collect();
        for change in changes {
            match change {
                big_embed::SqlAlter::Add(column) => {
                    if !fields.insert(column.name.clone()) {
                        return Err(local(big_db::DbError::FieldRedefined {
                            table: table.to_string(),
                            field: column.name.clone(),
                        }));
                    }
                }
                big_embed::SqlAlter::Drop(field) => {
                    if !fields.remove(field) {
                        return Err(local(big_db::DbError::UnknownField {
                            table: table.to_string(),
                            field: field.clone(),
                        }));
                    }
                }
            }
        }

        for change in changes {
            match change {
                big_embed::SqlAlter::Add(column) => self.create_column(table, column)?,
                big_embed::SqlAlter::Drop(field) => self.drop_field(table, field)? as u64,
            };
        }
        Ok(changes.len() as u64)
    }

    /// One column of a column list, as the field change it already is.
    ///
    /// The kind comes from `big_embed::introspect`, which owns that mapping in both directions -
    /// the other one is what `SHOW CREATE TABLE` writes a schema back out with. The three arms
    /// below are the three the field routes have, and they carry the same defaults: a decimal's
    /// scale is on the column because the parser refused one without it, and a time quantum
    /// takes the empty granularity that means the engine's default.
    fn create_column(&self, table: &str, column: &big_embed::SqlColumn) -> Result<u64> {
        let kind = big_embed::introspect::kind_of(column.kind);
        match kind {
            // `unwrap_or(0)` is unreachable - the parser refuses `DECIMAL` with no scale - and
            // is here rather than an `expect` because a panic on the schema path would take a
            // node down over a statement a client wrote.
            big_embed::FieldKind::Decimal => self.create_decimal(
                table,
                &column.name,
                column.bit_depth,
                column.scale.unwrap_or(0),
            ),
            big_embed::FieldKind::TimeQuantum => {
                self.create_time_quantum(table, &column.name, Vec::new())
            }
            _ => self.create_field(table, &column.name, kind, column.bit_depth),
        }
    }

    /// One SQL statement, translated and then run wherever it has to run.
    ///
    /// [`Cluster::classify`] then [`Cluster::run`], which is what a caller with no reason to
    /// look at the statement in between wants. An edge that authorises the statement first
    /// calls the two halves itself and passes the same value to both.
    pub fn sql(
        &self,
        text: &str,
        who: &big_rbac::Who,
        opts: &QueryOptions,
    ) -> Result<(ResultSet, Format)> {
        self.run(self.classify(text, opts)?, who, opts)
    }

    /// An already-classified statement, run wherever it has to run, answered as rows.
    ///
    /// **A `ResultSet` rather than plans and a shape, because only one of the five kinds of
    /// statement has either.** A query's rows come out of what the owners answered, applied to
    /// the shape `big-sql` decided; a schema change and an insert answer with a number they
    /// already know; a listing is read straight out of this node's catalog; an explanation is
    /// text. Assembling all five here is what lets the edge write bytes without knowing which
    /// one it got - and it is where the assembling belonged anyway, since a `Shape` is only
    /// right once every owner's answer is in.
    ///
    /// **Takes the statement, not the text.** The value handed in is the one the caller
    /// authorised, so nothing between the check and the work can re-read the bytes and reach a
    /// different conclusion about what they say.
    pub fn run(
        &self,
        sql: big_embed::Sql,
        who: &big_rbac::Who,
        opts: &QueryOptions,
    ) -> Result<(ResultSet, Format)> {
        // **Every demand, before any of the work.** The statement says what it needs next to the
        // variants it is about; this asks whether the caller holds all of it. A pure lookup over
        // integers against the catalog already in memory - it must never grow an I/O, because it
        // runs once per statement on the path where re-authenticating would cost a second argon2
        // hash.
        //
        // Ahead of the fan-out rather than inside it, so a refused statement reaches no peer and
        // takes no record ids from the leader.
        for demand in sql.demands() {
            if !self.api.allows(who, demand) {
                return Err(ClusterError::Local(big_embed::ApiError::Denied(Box::new(
                    big_rbac::Denied {
                        role: match who {
                            big_rbac::Who::Role(r) => Some(r.clone()),
                            big_rbac::Who::Trusted => None,
                        },
                        privilege: demand.privilege,
                        on: demand.on.to_owned(),
                    },
                ))));
            }
        }
        // The five kinds go to different places: a query is planned here and fanned out to the
        // owners, a schema change goes to the leader and then everywhere, an insert goes to the
        // shards that own its records, and a listing goes nowhere at all. Deciding it here
        // rather than inside `plan_sql` is what keeps a `CREATE TABLE` from being applied on
        // whichever node the client happened to reach - which is the failure a schema leader
        // exists to prevent.
        let statement = match sql {
            big_embed::Sql::Ddl(ddl) => return self.sql_ddl(&ddl),
            big_embed::Sql::Insert(insert) => return self.sql_insert(&insert, opts),
            big_embed::Sql::Show(show) => return self.sql_show(&show, who),
            // Replicated through the same leader-then-fan-out a schema change takes, because a
            // grant that reached two nodes of three is an intermittent refusal - which is the
            // failure nobody notices.
            big_embed::Sql::Acl(acl) => return self.sql_acl(&acl),
            // `EXPLAIN` reaches none of the three above and none of the fan-out below: what the
            // statement is has already been decided by the time it gets here, and writing it
            // out is the whole of the work.
            big_embed::Sql::Explain { mode, inner } => return self.sql_explain(mode, *inner),
            big_embed::Sql::Query(s) => s,
        };
        let started = Instant::now();
        // **The semi-joins first, and they fan out like everything else - which is the whole
        // reason they are here rather than in a plan.** `IN (SELECT _record_id FROM b ...)`
        // narrows this table by the ids `b` holds, and a per-node share of those ids would
        // narrow one node's records by a fraction of the set: a smaller answer that looks
        // exactly like a correct one. So the inner set is fanned out and merged in full before
        // the outer call is planned, the same way a join's arithmetic waits for both sides.
        let mut statement = statement;
        big_embed::resolve_sets(
            &mut statement.calls,
            |t, c| self.api.plan_call(t, c).map_err(ClusterError::Local),
            |p| self.execute(p, &big_embed::remaining(opts, started)),
            |_, _| {
                ClusterError::Local(big_embed::ApiError::Sql(big_embed::SqlError::Refused {
                    what: big_embed::Refused::SetTooLarge,
                    at: 0,
                }))
            },
        )?;
        let (plans, probes, answer) = self.api.plan_statement(statement)?;
        // One fan-out and one merge per plan, each exactly the fan-out and merge that plan
        // would have got written on its own. A statement that asks two questions costs two
        // round trips and teaches the layer below nothing.
        //
        // The timeout bounds the statement rather than each plan in it, which matters more here
        // than it does un-clustered: what is being held is a worker on every owner, not only on
        // this node. See `big_embed::remaining`.
        let mut values = Vec::with_capacity(plans.len());
        for plan in &plans {
            values.push(self.execute(plan, &big_embed::remaining(opts, started))?);
        }
        // Then the searches. Each step of one is an ordinary fan-out and merge, so a quantile
        // is exact across the cluster for the same reason it is exact on one node: what moves
        // the bound is the merged count, never a node's share of it.
        for probe in &probes {
            values.push(big_embed::run_probe(
                probe,
                |t, c| self.api.plan_call(t, c).map_err(ClusterError::Local),
                |p| self.execute(p, &big_embed::remaining(opts, started)),
            )?);
        }
        Ok((big_embed::result_set(&answer, &values), answer.format))
    }

    /// `EXPLAIN`: resolved as far as it would have to be to run, and then not run.
    ///
    /// **A query is planned and the other three are not**, which is not an optimisation - it is
    /// what those statements are. A plan is a statement put against a schema, so
    /// `EXPLAIN SELECT nope FROM tx` has to report the unknown field rather than draw a tree for
    /// a statement that could never run. A schema change, a write and a listing are already
    /// wholly in the parse tree, so explaining one reads no catalog at all - which is also what
    /// keeps an explained `CREATE` from becoming a way to ask whether a table exists.
    ///
    /// Nothing past the planning happens on any path: no `execute`, no probe, no round trip.
    fn sql_explain(
        &self,
        mode: big_embed::ExplainMode,
        inner: big_embed::Sql,
    ) -> Result<(ResultSet, Format)> {
        // Planned here, because a plan needs the catalog and the catalog is this side's. What
        // the plans then *say* is `big-sql`'s, which is why this function chooses no printer:
        // it resolves, and hands over.
        let (planned, format) = match inner {
            big_embed::Sql::Query(statement) => {
                // **A semi-join has no plan to draw until it has run, and this says so rather
                // than drawing a different one.** The outer call is narrowed by the ids the
                // inner set holds, so its tree is not a fact about the statement - it is a fact
                // about the other table's contents at the moment it was asked. Standing in an
                // `All()` would print a tree that is never the one that runs, and running the
                // inner set would break the one promise an `EXPLAIN` makes.
                if statement.calls.iter().any(|a| big_embed::has_set(&a.call)) {
                    return Err(ClusterError::Local(big_embed::ApiError::Sql(
                        big_embed::SqlError::Refused {
                            what: big_embed::Refused::ExplainSet,
                            at: 0,
                        },
                    )));
                }
                let (plans, probes, answer) = self.api.plan_statement(statement)?;
                // A search's records are an ordinary call, resolved so the tree under it is the
                // one the search would actually walk. Without this a statement that is only a
                // quantile - which makes no call at all - would explain as nothing but a shape,
                // hiding the `WHERE` that is the whole of its work.
                let probed = probes
                    .iter()
                    .map(|p| {
                        self.api
                            .plan_call(&p.table, &p.rows)
                            .map(|rows| big_embed::explain::Probed { probe: p, rows })
                            .map_err(ClusterError::Local)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let format = answer.format;
                let set = big_embed::explain::result_set(
                    mode,
                    &big_embed::explain::Explained::Query {
                        plans: &plans,
                        probes: &probed,
                        answer: &answer,
                    },
                );
                (set, format)
            }
            // The three that need no schema, so nothing here is resolved for them at all.
            big_embed::Sql::Ddl(d) => (
                big_embed::explain::result_set(mode, &big_embed::explain::Explained::Ddl(&d)),
                Format::default(),
            ),
            big_embed::Sql::Acl(a) => (
                big_embed::explain::result_set(mode, &big_embed::explain::Explained::Acl(&a)),
                Format::default(),
            ),
            big_embed::Sql::Insert(i) => (
                big_embed::explain::result_set(mode, &big_embed::explain::Explained::Insert(&i)),
                Format::default(),
            ),
            big_embed::Sql::Show(s) => (
                big_embed::explain::result_set(mode, &big_embed::explain::Explained::Show(&s)),
                s.format,
            ),
            // The parser refuses a second `EXPLAIN`, so nothing constructs this. Reported rather
            // than asserted: a panic here would take a node down over a statement a client
            // wrote, and an unreachable state is worth exactly one error path.
            big_embed::Sql::Explain { .. } => {
                return Err(ClusterError::Local(big_embed::ApiError::Sql(
                    big_embed::SqlError::Syntax {
                        at: 0,
                        found: "EXPLAIN".to_string(),
                        want: "a statement to explain",
                    },
                )))
            }
        };
        Ok((planned, format))
    }

    /// `INSERT`, which is `POST /table/{t}/import` with the facts written as a statement.
    ///
    /// **The whole batch is resolved against the schema before any of it is written**, which is
    /// what the import route does and for the same reason: a statement that turns out to name a
    /// field nobody has must not land halfway. What a value means is the field's kind to decide,
    /// and `big_embed::fact` is where both write paths ask - so `12.50` on a decimal of scale two
    /// is the same 1250 units however it arrived.
    ///
    /// A statement that named no `id` column is given a run of ids by the schema leader, before
    /// anything is sent anywhere - the same ordering interning follows, and for the same reason:
    /// what fails must fail while nothing has been written. The run is contiguous and taken in
    /// the order the rows were written, so `VALUES (…), (…)` reads back in the order it was
    /// typed.
    fn sql_insert(
        &self,
        insert: &big_embed::SqlInsert,
        opts: &QueryOptions,
    ) -> Result<(ResultSet, Format)> {
        // The qualified name, which is what every route below takes and what an error should
        // say back: `orders` is not the table that was not found, `sales.orders` is.
        let name = qualified(&insert.database, &insert.table);
        let target = big_db::TableRef::parse(&name);
        let schema = self.schema();
        let table = schema
            .iter()
            .find(|t| t.name == target.table && t.database == target.database)
            .ok_or_else(|| local(big_db::DbError::UnknownTable(name.clone())))?;

        // Resolved once per column rather than once per value: a statement writing ten thousand
        // rows names the same handful of fields over and over. **The schema's copy of the name,
        // not the statement's** - identical strings, so that `big_embed::apply` matches a fact to
        // its field by address rather than by `memcmp`, exactly as the import route arranges.
        let mut fields = Vec::with_capacity(insert.columns.len());
        for (i, column) in insert.columns.iter().enumerate() {
            if Some(i) == insert.id_at {
                continue;
            }
            let info = table.fields.iter().find(|f| &f.name == column).ok_or_else(|| {
                local(big_db::DbError::UnknownField { table: name.clone(), field: column.clone() })
            })?;
            fields.push(info);
        }

        // **`INSERT ... SELECT` becomes rows of literals, and then it is the statement above.**
        //
        // The query is run first, whole, through the same path a bare `SELECT` takes - so it is
        // planned, fanned out and merged exactly as it would have been on its own, and what
        // comes back is the finished answer rather than one node's share of it. Only then is
        // anything written. That ordering is what the import route follows too: what fails must
        // fail while nothing has been written.
        // Every cell as `Some(literal)` for the written form, and `None` where a source record
        // held no value in that field - which is written as no fact, because that is what "no
        // value" already means here.
        let rows: Vec<Vec<Option<big_embed::Literal>>> = match insert.select() {
            None => {
                insert.values().iter().map(|row| row.iter().cloned().map(Some).collect()).collect()
            }
            Some(select) => self.read_source(select, &name, opts)?,
        };

        // Asked for once for the whole statement rather than once per row: a round trip per
        // row would make a thousand-row insert a thousand round trips to one node.
        let allocated = match insert.id_at {
            Some(_) => 0,
            None => self.allocate(&name, rows.len() as u64)?,
        };

        let mut facts = Vec::with_capacity(rows.len() * insert.field_count());
        for (n, row) in rows.iter().enumerate() {
            // The id column, when the statement wrote one, is a literal by construction - the
            // parser refused anything else there - and the query form has none at all.
            let record = match insert.id_at.and_then(|i| row.get(i)) {
                Some(Some(big_embed::Literal::Int(id))) => *id,
                _ => allocated + n as u64,
            };
            let values = row.iter().enumerate().filter(|(i, _)| Some(*i) != insert.id_at);
            for (info, (_, value)) in fields.iter().zip(values) {
                // No value is no fact. A record that held nothing in the source field holds
                // nothing in the target one, which is the same statement rather than a zero.
                let Some(value) = value else { continue };
                facts.push(
                    big_embed::fact::from_literal(&info.name, info, record, value).map_err(
                        |e| {
                            // The mapping is `fact`'s, not this function's: a value with more
                            // digits than its field keeps is the planner's own refusal, and it
                            // reads the same here as it does in a `WHERE`.
                            ClusterError::Local(
                                e.into_error(&info.name, &big_embed::fact::written(value)),
                            )
                        },
                    )?,
                );
            }
        }

        // The same two-armed choice the import route makes: facts borrowed all the way in when
        // this node writes alone, owned when a batch has to be shipped.
        let outcome = if self.writes_alone() {
            self.import_borrowed(&name, &facts)?
        } else {
            let owned: Vec<_> = facts.iter().map(OwnedFact::from_fact).collect();
            self.import(&name, &owned)?
        };
        let _ = outcome;
        Ok((
            big_embed::one_cell("inserted", big_embed::Datum::Int(rows.len() as i128)),
            Format::default(),
        ))
    }

    /// The rows an `INSERT ... SELECT` writes, read out of the source table.
    ///
    /// The query goes through [`Self::run`] rather than through a private path, so that it is
    /// planned, fanned out, merged and shaped exactly as the same `SELECT` written on its own -
    /// which is what makes "insert what that query answers" a claim about one thing rather than
    /// about two implementations that have to agree.
    ///
    /// The cells then become literals, because that is what the write path takes: `big_embed::
    /// fact::from_literal` is the one place a value meets a field's kind, and routing this
    /// through it is what keeps `12.50` the same 1250 units however it arrived. See
    /// [`big_embed::literal_of`] for the one cell shape that has no literal spelling.
    fn read_source(
        &self,
        select: &big_embed::SqlSelect,
        target: &str,
        opts: &QueryOptions,
    ) -> Result<Vec<Vec<Option<big_embed::Literal>>>> {
        let query = big_embed::SqlQuery { branches: vec![select.clone()] };
        // Unreachable through the parser, which accepts nothing here that would fail to lower.
        // Carried rather than asserted: a panic would take a node down over a statement a
        // client wrote.
        let statement = big_embed::lower(&query)
            .map_err(|e| ClusterError::Local(big_embed::ApiError::Sql(e)))?;
        // Trusted: the outer statement's demands covered this table before any of this ran, and
        // asking again here would refuse a read the caller was already allowed to make.
        let (set, _) = self.run(big_embed::Sql::Query(statement), &big_rbac::Who::Trusted, opts)?;

        let mut out = Vec::with_capacity(set.rows.len());
        for row in &set.rows {
            let mut literals = Vec::with_capacity(row.len());
            for (cell, column) in row.iter().zip(&set.columns) {
                // The column is named in `table` because that is the only field here that
                // carries a `String`, and which column cannot be read is the whole of what the
                // reader needs. `what` says which kind of value it was.
                literals.push(big_embed::literal_of(cell).map_err(|what| {
                    local(big_db::DbError::EngineCannotAnswer {
                        table: format!("{target}`, column `{column}"),
                        what,
                        engine: "bitmap",
                        instead: "a value on the write path travels as a literal, and a literal \
                                  here is exact - an integer, or an integer and a scale. There \
                                  is no exact spelling of a float, and the nearest decimal is a \
                                  different number. Select the other columns, or read this one \
                                  out and write it back with `POST /table/{t}/import`",
                    })
                })?);
            }
            out.push(literals);
        }
        Ok(out)
    }

    /// `DESCRIBE` and `SHOW`, answered out of this node's catalog - which is every node's.
    /// `GRANT`, `REVOKE`, `CREATE ROLE`, `DROP ROLE`.
    ///
    /// **The resulting mask is computed here, at the coordinator, and the answer is what
    /// travels.** A peer applying a delta would have to read its own grants to know what the
    /// result should be, and two nodes reading two states is how they end up disagreeing about
    /// who may do what. See `wire::Ddl::SetGrant`.
    ///
    /// ⚠️ A partial failure matters more here than for a schema change. A `REVOKE` that reached
    /// two nodes of three leaves the privilege live on the third, which in a load-balanced pool
    /// is an intermittent success where a refusal was wanted - the failure mode nobody notices.
    /// `ClusterError::Partial` names the nodes; a revoke reported partial must be re-run until
    /// it is not.
    fn sql_acl(&self, acl: &big_embed::SqlAcl) -> Result<(ResultSet, Format)> {
        let (column, changed) = match acl {
            big_embed::SqlAcl::CreateRole { name, if_not_exists } => {
                if *if_not_exists && self.api.roles().iter().any(|r| r == name) {
                    ("role", 0u64)
                } else {
                    ("role", self.ddl(&Ddl::CreateRole { role: name.clone() })?)
                }
            }
            big_embed::SqlAcl::DropRole { name, if_exists } => {
                if *if_exists && !self.api.roles().iter().any(|r| r == name) {
                    ("dropped", 0)
                } else {
                    ("dropped", self.ddl(&Ddl::DropRole { role: name.clone() })?)
                }
            }
            big_embed::SqlAcl::Grant { privileges, on, role } => {
                ("granted", self.set_grant(role, on, *privileges, true)?)
            }
            big_embed::SqlAcl::Revoke { privileges, on, role } => {
                ("revoked", self.set_grant(role, on, *privileges, false)?)
            }
        };
        Ok((big_embed::one_cell(column, big_embed::Datum::Int(changed.into())), Format::default()))
    }

    /// Reads what the role holds on the object, applies the change, and sends the result.
    ///
    /// `add` is the difference between `GRANT` and `REVOKE`, and it is the only one: both end in
    /// one absolute mask, so neither can be applied twice to a different effect.
    fn set_grant(
        &self,
        role: &str,
        on: &big_embed::SqlAclObject,
        privileges: big_rbac::Privileges,
        add: bool,
    ) -> Result<u64> {
        let object = on.resolved();
        let (database, table) = match &object {
            big_rbac::Object::Server => (String::new(), String::new()),
            big_rbac::Object::Database(d) => (d.clone(), String::new()),
            big_rbac::Object::Table { database, table } => (database.clone(), table.clone()),
        };
        let held = self.api.granted(role, &object).map_err(ClusterError::Local)?;
        let result = if add { held.union(privileges) } else { held.minus(privileges) };
        if result == held {
            return Ok(0);
        }
        self.ddl(&Ddl::SetGrant { role: role.to_string(), database, table, privileges: result.0 })
    }

    fn sql_show(
        &self,
        show: &big_embed::SqlShow,
        who: &big_rbac::Who,
    ) -> Result<(ResultSet, Format)> {
        let schema = self.schema();
        // This node's own views, for the same reason the schema is this node's own: a listing
        // is read out of the catalog every node holds, and a schema change reached all of them
        // before it was answered.
        let views = self.api.views();
        let set = match &show.what {
            big_embed::SqlShown::Columns { database, table } => {
                big_embed::introspect::describe(&schema, &views, &qualified(database, table))
            }
            big_embed::SqlShown::Tables { database } => {
                Ok(big_embed::introspect::show_tables(&schema, &views, database.as_deref()))
            }
            big_embed::SqlShown::Views { database } => {
                Ok(big_embed::introspect::show_views(&views, database.as_deref()))
            }
            big_embed::SqlShown::Databases => Ok(big_embed::introspect::show_databases(&schema)),
            big_embed::SqlShown::Roles => Ok(big_embed::introspect::show_roles(&self.api.roles())),
            // A bare `SHOW GRANTS` is about the caller's own role, which is why it needs no
            // privilege: reading what you hold tells you nothing you could not find out by
            // trying. A `Trusted` caller holds everything and has no role to name, so it gets
            // the empty answer rather than a guess.
            big_embed::SqlShown::Grants { role } => {
                let named = match (role, who) {
                    (Some(r), _) => Some(r.clone()),
                    (None, big_rbac::Who::Role(r)) => Some(r.clone()),
                    (None, big_rbac::Who::Trusted) => None,
                };
                Ok(big_embed::introspect::show_grants(
                    &named.map(|r| self.api.grants_of(&r)).unwrap_or_default(),
                ))
            }
            big_embed::SqlShown::Create { database, table, view } => {
                big_embed::introspect::show_create(
                    &schema,
                    &views,
                    &qualified(database, table),
                    *view,
                )
            }
        };
        Ok((set.map_err(ClusterError::Local)?, show.format))
    }

    /// Fans a plan out to every owner and merges what comes back.
    ///
    /// The plan travels, not the text. Re-parsing per node would let two nodes disagree about
    /// what was asked, and a result cannot show that it happened.
    pub fn execute(&self, plan: &Plan, opts: &QueryOptions) -> Result<Value> {
        // **The one plan that is asked more than once.** See `top_n_is_certain`: an owner cut to
        // `n` is wrong, an owner asked for everything is right and can ship a million groups, and
        // between them is a bound that says when it has already been asked for enough.
        if let Plan::TopN { n, .. } = plan {
            return self.execute_top_n(plan, *n, opts);
        }
        self.execute_asked(plan, &asked_of_owners(plan), opts)
    }

    /// A `TopN`, asked for a widening bound until the merge is provably the whole table's.
    ///
    /// The last round asks for everything, which is what this replaced - so the worst case is
    /// the old behaviour plus the rounds before it, and the best case is one round of `n + 4`
    /// groups per owner instead of every group each of them holds.
    fn execute_top_n(&self, plan: &Plan, n: usize, opts: &QueryOptions) -> Result<Value> {
        let Plan::TopN { table, rows, field, .. } = plan else {
            unreachable!("only a `TopN` reaches here")
        };
        let bound = |m: usize| Plan::TopN {
            table: table.clone(),
            rows: rows.clone(),
            field: field.clone(),
            n: m,
        };
        for extra in TOP_N_ROUNDS {
            let asked = n.saturating_add(extra);
            let answers = self.ask_owners(&bound(asked), opts)?;
            let reports: Vec<Reported> =
                answers.iter().map(|(_, v)| Reported::of(v, asked)).collect();
            if top_n_is_certain(&reports, n) {
                return self.merge_answers(plan, answers);
            }
        }
        // Everything, which is where this started and is always correct.
        let answers = self.ask_owners(&bound(usize::MAX), opts)?;
        self.merge_answers(plan, answers)
    }

    /// One fan-out of one plan, and the merge that follows it.
    fn execute_asked(&self, plan: &Plan, asked: &Plan, opts: &QueryOptions) -> Result<Value> {
        let answers = self.ask_owners(asked, opts)?;
        self.merge_answers(plan, answers)
    }

    fn merge_answers(&self, plan: &Plan, answers: Vec<(usize, Value)>) -> Result<Value> {
        let mut merge = Merge::new(plan);
        for (i, value) in answers {
            merge.add(&self.describe(i), value)?;
        }
        Ok(merge.finish())
    }

    /// Every primary owner's answer to one plan, exactly as it was asked.
    fn ask_owners(&self, asked: &Plan, opts: &QueryOptions) -> Result<Vec<(usize, Value)>> {
        let asked = asked.clone();
        let timeout_ms = opts.timeout.map(|t| t.as_millis().min(u64::MAX as u128) as u64);

        // Primaries only. A replica holds the same records, so asking it as well would double
        // every count - and choosing it *instead* would answer from a copy that this node
        // cannot know is current.
        //
        // **And each is asked for its own range only.** Once a node can serve two of them, a
        // body that did not name one would have that node answer with both, twice - so the
        // scope travels with the plan rather than being inferred from who received it.
        let answers = self.fan_out_over(
            &self.candidates(0..self.range_count()),
            opts.timeout,
            path::QUERY,
            |slot| {
                wire::QueryRequest {
                    plan: asked.clone(),
                    timeout_ms,
                    shards: self.scope_of_range(slot),
                }
                .encode()
            },
            wire::decode_value,
            |slot| {
                self.guard()?;
                let opts = opts.clone().in_shards(self.scope_of_range(slot));
                self.api.execute(&asked, &opts).map_err(ClusterError::Local)
            },
        )?;
        Ok(answers)
    }

    /// A page of record ids from the whole cluster, ascending.
    ///
    /// Every owner is asked for a page of the same size and the coordinator keeps the first
    /// `limit` of the merge. An owner whose whole range sits below the cursor is not asked at
    /// all: it has nothing that could come after it, and skipping it is the difference between
    /// a scan costing the cluster and a scan costing the part of it that is still in play.
    pub fn records(
        &self,
        table: &str,
        after: Option<RecordId>,
        limit: usize,
    ) -> Result<Vec<RecordId>> {
        // The ranges still in play, and the slot order they were taken in. A slot is an index
        // into *this* list once a range has been pruned, so the range it stands for has to be
        // carried rather than assumed - otherwise a pruned scan would scope every remaining
        // owner to the wrong shards.
        let live: Vec<usize> =
            (0..self.range_count()).filter(|r| self.may_hold_after(*r, after)).collect();
        let asked = self.candidates(live.iter().copied());

        let answers = self.fan_out_over(
            &asked,
            None,
            path::RECORDS,
            |slot| {
                wire::RecordsRequest {
                    table: table.to_string(),
                    after,
                    limit: limit.min(u64::MAX as usize) as u64,
                    shards: self.scope_of_range(live[slot]),
                }
                .encode()
            },
            wire::get_records,
            |slot| {
                self.guard()?;
                self.api
                    .records_in(table, after, limit, self.scope_of_range(live[slot]))
                    .map_err(ClusterError::Local)
            },
        )?;

        Ok(merge::merge_records(answers.into_iter().map(|(_, page)| page).collect(), limit))
    }
}

/// What each owner is actually asked, which is not always what the client asked.
///
/// One rewrite, and it is the one the design called out: a `TopN` cut to `n` at each owner is
/// wrong, because a group that is merely second everywhere would be dropped before anything
/// could add its contributions up. Each owner is asked for *all* of its groups and the
/// coordinator cuts the merged list, which is the simplest rule that is correct. A tighter
/// bound - asking for `n` plus some function of the node count - is possible and is an
/// optimisation, not a correction.
///
/// Everything else travels unchanged, because everything else is already order-independent:
/// `Count`, `Sum`, `Min` and `Max` fold, and `Distinct` and `GroupBy` return every group they
/// have by construction.
fn asked_of_owners(plan: &Plan) -> Plan {
    match plan {
        Plan::TopN { table, rows, field, .. } => Plan::TopN {
            table: table.clone(),
            rows: rows.clone(),
            field: field.clone(),
            n: usize::MAX,
        },
        // Everything else travels as it is, including `Project`: each owner is asked for the
        // whole page because the first `limit` records overall can all live on one node, and
        // the cut happens once in the merge. A `TopN` is the exception above because its cut is
        // a *ranking* - a group that leads nowhere can still lead everywhere once the shards
        // are added up, so no owner may drop one.
        other => other.clone(),
    }
}

/// `database.table`, or the bare table when the statement did not name a database.
///
/// The one string form a table travels as - see `big_db::TableRef::parse`, which reads it back.
fn qualified(database: &Option<String>, table: &str) -> String {
    match database {
        Some(d) => format!("{d}.{table}"),
        None => table.to_string(),
    }
}

/// The bound each owner is asked for before a `TopN` can be answered from less than everything.
///
/// **Why there has to be a bound at all.** A group that leads nowhere can still lead everywhere
/// once the shards are added up, so no owner may cut to `n` - the simplest correct rule is to
/// ask every owner for *all* of its groups, and that is what this replaces. On a column with a
/// million distinct values, a `LIMIT 10` then ships a million groups from every node.
///
/// **Why a bound can be safe.** An owner asked for its top `m` also says, by the count of the
/// smallest group it returned, that everything it did *not* return is at or below that number.
/// Call that its threshold. A group nobody returned therefore has a total no larger than the
/// thresholds added up, and a group some returned has a total no larger than what is known plus
/// the thresholds of the owners that stayed quiet. When both of those bounds fall below the
/// `n`-th known total, nothing unseen can enter the answer and the merge is already exact.
///
/// When they do not, the bound widens and the round is asked again - and the widening ends at
/// everything, which is where it started. So this is never wrong and never worse than one extra
/// round of the old behaviour; it is only ever cheaper, and it is cheapest exactly where the old
/// rule was most expensive.
const TOP_N_ROUNDS: [usize; 3] = [4, 32, 512];

/// One round's answer from one owner, as the certainty test needs it.
struct Reported {
    /// The groups, by the row each is keyed on. Row ids are cluster-wide for a keyed field -
    /// the schema leader interns them - which is what lets two owners' groups be the same group.
    counts: BTreeMap<GroupAt, u64>,
    /// What this owner says about everything it did not return: at or below this. Zero when it
    /// returned fewer groups than it was asked for, because then it returned all it has.
    threshold: u64,
}

impl Reported {
    fn of(value: &Value, asked: usize) -> Self {
        let groups = value.as_groups().unwrap_or(&[]);
        let counts =
            groups.iter().map(|g| (g.at, group_count(g))).collect::<BTreeMap<GroupAt, u64>>();
        // **Fewer groups than asked for means there are no others**, so nothing is hidden and
        // the threshold is zero. Exactly as many is treated as if there were more, which costs
        // at most one extra round and can never be wrong in the other direction.
        let threshold = if groups.len() < asked {
            0
        } else {
            groups.iter().map(group_count).min().unwrap_or(0)
        };
        Self { counts, threshold }
    }
}

fn group_count(g: &big_embed::Group) -> u64 {
    match g.value.as_ref() {
        Value::Count(n) => *n,
        _ => 0,
    }
}

/// Whether the answer merged from these owners is already the one the whole table would give.
///
/// Three things have to hold, and each of them is about a different way a bounded round can be
/// short. Written out rather than folded together, because a bound that is subtly too generous
/// is a plausible wrong answer that nothing in the result could reveal.
fn top_n_is_certain(reports: &[Reported], n: usize) -> bool {
    if n == 0 {
        return true;
    }
    let total_threshold: u64 = reports.iter().map(|r| r.threshold).sum();
    // Every owner returned everything it has: there is nothing left to be uncertain about,
    // whatever the numbers say.
    if total_threshold == 0 {
        return true;
    }

    // What is known about each group so far, and what it could still gain from the owners that
    // did not mention it.
    let mut known: BTreeMap<GroupAt, u64> = BTreeMap::new();
    for report in reports {
        for (row, count) in &report.counts {
            *known.entry(*row).or_insert(0) += *count;
        }
    }
    let unseen_gain = |row: GroupAt| -> u64 {
        reports.iter().filter(|r| !r.counts.contains_key(&row)).map(|r| r.threshold).sum()
    };

    let mut ranked: Vec<(GroupAt, u64)> = known.into_iter().collect();
    // Descending by count, then by row - the same order `rank_top_n` puts them in, minus the
    // key, which is a tie-break this cannot see and does not need: what is being decided here
    // is only *which* groups are in play.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // Fewer candidates than asked for, and some owner is still holding groups back.
    if ranked.len() < n {
        return false;
    }
    let (top, rest) = ranked.split_at(n);
    let cut = top[n - 1].1;

    // 1. Every group in the answer has to have an *exact* count, not merely the largest known
    //    one: an owner that did not mention it may still hold records under it, and the number
    //    this returns is the number a client reads.
    if top.iter().any(|(row, _)| unseen_gain(*row) > 0) {
        return false;
    }
    // 2. No group that was seen but did not make the cut may be able to climb into it.
    if rest.iter().any(|(row, count)| count + unseen_gain(*row) >= cut) {
        return false;
    }
    // 3. Nor may a group no owner returned at all, whose total is bounded by the thresholds
    //    added up. Compared with `>=` rather than `>` for the reason the last one is: a tie at
    //    the cut is broken by a key this test cannot see, so a tie is not a certainty.
    total_threshold < cut
}

#[cfg(test)]
mod top_n_tests {
    use super::*;

    /// One owner's round, as counts by row and the threshold it implies.
    fn report(counts: &[(RowId, u64)], threshold: u64) -> Reported {
        Reported { counts: counts.iter().map(|(r, n)| (GroupAt::Row(*r), *n)).collect(), threshold }
    }

    /// Every owner returned everything it has, so there is nothing left to be uncertain about.
    #[test]
    fn no_threshold_anywhere_is_certain_whatever_the_counts_say() {
        let reports = [report(&[(1, 10), (2, 9)], 0), report(&[(2, 8)], 0)];
        assert!(top_n_is_certain(&reports, 1));
        assert!(top_n_is_certain(&reports, 5));
    }

    /// **A group in the answer needs an exact count, not merely the largest known one.**
    ///
    /// Row 1 leads on the first owner and the second never mentioned it - but the second is
    /// still holding groups worth up to three, so the number this would return is a number the
    /// client would read as final and it is not.
    #[test]
    fn a_leader_no_owner_confirmed_is_not_certain_even_when_it_cannot_be_beaten() {
        let reports = [report(&[(1, 100)], 3), report(&[(2, 4), (3, 3)], 3)];
        assert!(!top_n_is_certain(&reports, 1));
        // The same owners with nothing held back: now it is exact.
        let settled = [report(&[(1, 100)], 0), report(&[(2, 4), (3, 3)], 0)];
        assert!(top_n_is_certain(&settled, 1));
    }

    /// A group that missed the cut but could still climb into it.
    ///
    /// Row 2 is known to hold nine, and the owner that did not mention it is holding groups
    /// worth up to two - which is eleven against a cut of ten. **The threshold only bounds what
    /// an owner did not say**, so a group both owners confirmed cannot climb however large their
    /// thresholds are; this is the case where one of them stayed quiet.
    #[test]
    fn a_candidate_an_owner_stayed_quiet_about_can_still_reach_the_cut() {
        let reports = [report(&[(1, 10), (2, 9)], 2), report(&[(1, 0)], 2)];
        assert!(!top_n_is_certain(&reports, 1));

        // The same shape with that owner holding nothing back: row 2 is exact at nine, which
        // is below the cut, and nothing unseen can reach it either.
        let settled = [report(&[(1, 10), (2, 9)], 2), report(&[(1, 0)], 0)];
        assert!(top_n_is_certain(&settled, 1));
    }

    /// A threshold bounds only what its owner did not say.
    ///
    /// Both owners confirmed both groups, so neither number can grow - and a threshold of two
    /// against a cut of ten leaves nothing unseen that could reach it either.
    #[test]
    fn a_group_every_owner_confirmed_cannot_climb_however_large_the_thresholds() {
        let reports = [report(&[(1, 10), (2, 9)], 2), report(&[(1, 0), (2, 0)], 2)];
        assert!(top_n_is_certain(&reports, 1));
    }

    /// A group no owner returned, whose total is bounded by the thresholds added up.
    #[test]
    fn an_unseen_group_that_could_beat_the_cut_is_not_certain() {
        // Both owners confirmed row 1 at four apiece, so its count is exact at eight. But each
        // is still holding groups worth up to five, and ten beats eight.
        let reports = [report(&[(1, 4)], 5), report(&[(1, 4)], 5)];
        assert!(!top_n_is_certain(&reports, 1));

        // Drop what they are holding below the cut and the same shape is certain.
        let reports = [report(&[(1, 4)], 3), report(&[(1, 4)], 3)];
        assert!(top_n_is_certain(&reports, 1));
    }

    /// Fewer candidates than asked for, while an owner is still holding groups back.
    #[test]
    fn too_few_candidates_is_not_certain_unless_that_is_all_there_is() {
        let reports = [report(&[(1, 9)], 4), report(&[(1, 1)], 4)];
        assert!(!top_n_is_certain(&reports, 3));
        let settled = [report(&[(1, 9)], 0), report(&[(1, 1)], 0)];
        assert!(top_n_is_certain(&settled, 3));
    }

    /// Nothing was asked for, so nothing can be missing.
    #[test]
    fn a_top_none_is_certain() {
        assert!(top_n_is_certain(&[report(&[(1, 9)], 4)], 0));
    }
}
