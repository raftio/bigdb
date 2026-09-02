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
use big_embed::{Format, ResultSet};

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
    /// The edge needs the answer before it can decide what the request costs: `POST /sql` is
    /// authorised as `read`, and a schema change written in SQL needs `admin` - see
    /// [`big_embed::Sql::authority`]. Exposed here rather than having the edge parse for itself,
    /// so that what decides the role and what decides the action are one definition.
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
    pub fn sql(&self, text: &str, opts: &QueryOptions) -> Result<(ResultSet, Format)> {
        self.run(self.classify(text, opts)?, opts)
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
    pub fn run(&self, sql: big_embed::Sql, opts: &QueryOptions) -> Result<(ResultSet, Format)> {
        // The five kinds go to different places: a query is planned here and fanned out to the
        // owners, a schema change goes to the leader and then everywhere, an insert goes to the
        // shards that own its records, and a listing goes nowhere at all. Deciding it here
        // rather than inside `plan_sql` is what keeps a `CREATE TABLE` from being applied on
        // whichever node the client happened to reach - which is the failure a schema leader
        // exists to prevent.
        let statement = match sql {
            big_embed::Sql::Ddl(ddl) => return self.sql_ddl(&ddl),
            big_embed::Sql::Insert(insert) => return self.sql_insert(&insert),
            big_embed::Sql::Show(show) => return self.sql_show(&show),
            // `EXPLAIN` reaches none of the three above and none of the fan-out below: what the
            // statement is has already been decided by the time it gets here, and writing it
            // out is the whole of the work.
            big_embed::Sql::Explain { mode, inner } => return self.sql_explain(mode, *inner),
            big_embed::Sql::Query(s) => s,
        };
        let (plans, probes, answer) = self.api.plan_statement(statement)?;
        // One fan-out and one merge per plan, each exactly the fan-out and merge that plan
        // would have got written on its own. A statement that asks two questions costs two
        // round trips and teaches the layer below nothing.
        //
        // The timeout bounds the statement rather than each plan in it, which matters more here
        // than it does un-clustered: what is being held is a worker on every owner, not only on
        // this node. See `big_embed::remaining`.
        let started = Instant::now();
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
    fn sql_insert(&self, insert: &big_embed::SqlInsert) -> Result<(ResultSet, Format)> {
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

        // Asked for once for the whole statement rather than once per row: a round trip per
        // row would make a thousand-row insert a thousand round trips to one node.
        let allocated = match insert.id_at {
            Some(_) => 0,
            None => self.allocate(&name, insert.rows.len() as u64)?,
        };

        let mut facts = Vec::with_capacity(insert.fact_count());
        for (n, row) in insert.rows.iter().enumerate() {
            let record = insert.record(row).unwrap_or(allocated + n as u64);
            for (info, (_, value)) in fields.iter().zip(insert.facts(row)) {
                facts.push(big_embed::fact::from_literal(&info.name, info, record, value).map_err(
                    |e| {
                        // The mapping is `fact`'s, not this function's: a value with more
                        // digits than its field keeps is the planner's own refusal, and it
                        // reads the same here as it does in a `WHERE`.
                        ClusterError::Local(
                            e.into_error(&info.name, &big_embed::fact::written(value)),
                        )
                    },
                )?);
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
            big_embed::one_cell("inserted", big_embed::Datum::Int(insert.rows.len() as i128)),
            Format::default(),
        ))
    }

    /// `DESCRIBE` and `SHOW`, answered out of this node's catalog - which is every node's.
    fn sql_show(&self, show: &big_embed::SqlShow) -> Result<(ResultSet, Format)> {
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
        let asked = asked_of_owners(plan);
        let body = wire::QueryRequest {
            plan: asked.clone(),
            timeout_ms: opts.timeout.map(|t| t.as_millis().min(u64::MAX as u128) as u64),
        }
        .encode();

        // Primaries only. A replica holds the same records, so asking it as well would double
        // every count - and choosing it *instead* would answer from a copy that this node
        // cannot know is current. Which copy is authoritative is a fact about the config file
        // rather than about the moment, because there is no protocol here that could make it
        // one about the moment.
        let answers = self.fan_out_over(
            &self.candidates(0..self.config.range_count()),
            opts.timeout,
            path::QUERY,
            &body,
            wire::decode_value,
            || {
                self.guard()?;
                self.api.execute(&asked, opts).map_err(ClusterError::Local)
            },
        )?;

        let mut merge = Merge::new(plan);
        for (i, value) in answers {
            merge.add(&self.config.nodes()[i].name, value)?;
        }
        Ok(merge.finish())
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
        let body = wire::RecordsRequest {
            table: table.to_string(),
            after,
            limit: limit.min(u64::MAX as usize) as u64,
        }
        .encode();

        let asked = self
            .candidates((0..self.config.range_count()).filter(|r| self.may_hold_after(*r, after)));

        let answers =
            self.fan_out_over(&asked, None, path::RECORDS, &body, wire::get_records, || {
                self.guard()?;
                self.api.records(table, after, limit).map_err(ClusterError::Local)
            })?;

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
