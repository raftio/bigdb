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

    /// The same for one SQL statement, which is planned here and fanned out as a plan.
    ///
    /// **No fan-out code and no merge arm.** SQL reaches storage as the `Plan` the other
    /// surface produces, so everything below this line has nothing to learn - which was the
    /// condition the SQL surface was built under rather than a happy accident of it.
    /// Which kind of statement this is, without planning or running it.
    ///
    /// The edge needs the answer before it can decide what role the request needs: `POST /sql`
    /// is authorised as `read`, and a schema change written in SQL needs `admin`. Exposed here
    /// rather than having the edge parse for itself, so that what decides the role and what
    /// decides the action are one definition.
    pub fn classify(&self, text: &str) -> big_api::Result<big_api::Sql> {
        self.api.translate(text)
    }

    /// A schema change written in SQL, applied the way every schema change is.
    ///
    /// Answers with the table's id in a one-cell row, so that a client reading `/sql` gets a
    /// result set rather than a second response shape to learn. The id is the leader's, which is
    /// the same number `POST /table/{t}` answers with and for the same reason.
    fn sql_ddl(&self, ddl: &big_api::SqlDdl) -> Result<(Vec<Value>, Answer)> {
        let big_api::SqlDdl::CreateTable { table, engine } = ddl;
        // The engine list lives in one place, and it is not `big-sql`: that crate links no
        // storage and takes the name as written, so this is where a name nobody has is refused.
        let engine = match engine {
            None => big_api::TableEngine::default(),
            Some(name) => big_api::TableEngine::parse(name).ok_or_else(|| {
                ClusterError::Local(big_db::DbError::UnknownEngineName(name.clone()).into())
            })?,
        };
        let id = self.create_table_with(table, engine)?;
        Ok((
            vec![Value::Count(id)],
            Answer {
                shape: big_api::Shape::Row {
                    cells: vec![big_api::Cell {
                        column: "table".to_string(),
                        of: big_api::Of::Value { plan: 0 },
                    }],
                },
                format: big_api::Format::default(),
                calls: 1,
            },
        ))
    }

    pub fn sql(&self, text: &str, opts: &QueryOptions) -> Result<(Vec<Value>, Answer)> {
        // Classified before anything is planned, because the two kinds go to different places:
        // a query is planned here and fanned out to the owners, a schema change goes to the
        // leader and then everywhere. Deciding it here rather than inside `plan_sql` is what
        // keeps a `CREATE TABLE` from being applied on whichever node the client happened to
        // reach - which is the failure a schema leader exists to prevent.
        let statement = match self.api.translate(text)? {
            big_api::Sql::Ddl(ddl) => return self.sql_ddl(&ddl),
            big_api::Sql::Query(s) => s,
        };
        let (plans, probes, answer) = self.api.plan_statement(statement)?;
        // One fan-out and one merge per plan, each exactly the fan-out and merge that plan
        // would have got written on its own. A statement that asks two questions costs two
        // round trips and teaches the layer below nothing.
        //
        // The timeout bounds the statement rather than each plan in it, which matters more here
        // than it does un-clustered: what is being held is a worker on every owner, not only on
        // this node. See `big_api::remaining`.
        let started = Instant::now();
        let mut values = Vec::with_capacity(plans.len());
        for plan in &plans {
            values.push(self.execute(plan, &big_api::remaining(opts, started))?);
        }
        // Then the searches. Each step of one is an ordinary fan-out and merge, so a quantile
        // is exact across the cluster for the same reason it is exact on one node: what moves
        // the bound is the merged count, never a node's share of it.
        for probe in &probes {
            values.push(big_api::run_probe(
                probe,
                |t, c| self.api.plan_call(t, c).map_err(ClusterError::Local),
                |p| self.execute(p, &big_api::remaining(opts, started)),
            )?);
        }
        Ok((values, answer))
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
