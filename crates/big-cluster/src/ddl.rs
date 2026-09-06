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

//! Schema changes, decided in one place and applied everywhere.
//!
//! The leader rules on legality - a taken name, a contradicting field kind, a table that is not
//! there to drop - before any other node has heard of the change. What the other nodes do is
//! apply something already ruled legal.
//!
//! No field ids travel. They are each node's own numbering; names do the resolving.

use super::*;

impl<P: PagerMut + Sync> Cluster<P> {
    /// One schema change, at one node.
    pub(super) fn tell(&self, node: usize, op: &Ddl) -> Result<()> {
        if node == self.config.this_index() {
            return apply_ddl(&self.api, op).map(|_| ()).map_err(ClusterError::Local);
        }
        self.ask(node, path::DDL, &op.encode(), None).map(|_| ())
    }

    // -----------------------------------------------------------------------------------
    // Schema
    // -----------------------------------------------------------------------------------

    /// The schema, as this node holds it.
    ///
    /// Every node's copy is the same because every schema change is applied everywhere, and
    /// the leader is the one that decides whether a change is legal at all.
    pub fn schema(&self) -> Vec<big_embed::TableInfo> {
        self.api.schema()
    }

    /// Every view this node holds, with the statement each carries.
    ///
    /// This node's own, like [`Cluster::schema`]: a schema change reaches every node before it
    /// is answered, so a listing has no one to ask.
    pub fn views(&self) -> Vec<big_embed::ViewInfo> {
        self.api.views()
    }

    /// Creates a table under the default engine.
    pub fn create_table(&self, table: &str) -> Result<u64> {
        self.create_table_with(table, big_embed::TableEngine::default())
    }

    /// The same, with the storage engine named.
    ///
    /// The engine is part of the change rather than each node's own decision: two copies of one
    /// table that stored different things would answer the same query at different costs, and a
    /// failover would change a query's cost without changing the query.
    pub fn create_table_with(&self, table: &str, engine: big_embed::TableEngine) -> Result<u64> {
        self.ddl(&Ddl::CreateTable { table: table.to_string(), engine })
    }

    pub fn create_field(
        &self,
        table: &str,
        field: &str,
        kind: big_embed::FieldKind,
        bit_depth: u32,
    ) -> Result<u64> {
        self.ddl(&Ddl::CreateField {
            table: table.to_string(),
            field: field.to_string(),
            kind,
            bit_depth,
        })
    }

    pub fn create_decimal(
        &self,
        table: &str,
        field: &str,
        bit_depth: u32,
        scale: i8,
    ) -> Result<u64> {
        self.ddl(&Ddl::CreateDecimal {
            table: table.to_string(),
            field: field.to_string(),
            bit_depth,
            scale,
        })
    }

    pub fn create_time_quantum(
        &self,
        table: &str,
        field: &str,
        granularity: Vec<big_embed::Granularity>,
    ) -> Result<u64> {
        self.ddl(&Ddl::CreateTimeQuantum {
            table: table.to_string(),
            field: field.to_string(),
            granularity,
        })
    }

    pub fn create_database(&self, name: &str) -> Result<bool> {
        self.ddl(&Ddl::CreateDatabase { name: name.to_string() }).map(|n| n == 1)
    }

    /// Removes a database everywhere, with `cascade` to take its tables with it.
    ///
    /// `Ok(false)` means there was no such database.
    ///
    /// **The emptiness check happens here, at the leader, and only here.** A peer applying the
    /// change re-deciding it against its own table count would be a second opinion on a
    /// question that already has an answer - and if the two ever differed, half the cluster
    /// would drop the database and half would refuse. So what travels is always the cascading
    /// form: a change already ruled legal. See [`Ddl::DropDatabase`].
    pub fn drop_database_if_empty(&self, name: &str, cascade: bool) -> Result<bool> {
        if name == big_db::DEFAULT_DATABASE_NAME {
            return Err(ClusterError::Local(big_embed::ApiError::Db(
                big_db::DbError::DropDefaultDatabase,
            )));
        }
        // Views count too, the same way they do in `Catalog::drop_database`: a `RESTRICT` that
        // was about tables only would drop a database still holding statements somebody wrote.
        let held = self.schema().iter().filter(|t| t.database == name).count()
            + self.api.views().iter().filter(|v| v.database == name).count();
        if held > 0 && !cascade {
            return Err(ClusterError::Local(big_embed::ApiError::Db(
                big_db::DbError::DatabaseNotEmpty { database: name.to_string(), tables: held },
            )));
        }
        self.ddl(&Ddl::DropDatabase { name: name.to_string() }).map(|n| n == 1)
    }

    /// Stores a `SELECT` under a name everywhere.
    ///
    /// **`IF NOT EXISTS` and `OR REPLACE` are both decided here and only here**, for the reason
    /// [`Cluster::drop_database_if_empty`] states: the definition that is already there is the
    /// leader's to see, and a peer re-deciding against its own copy could refuse a change the
    /// leader has already made. What travels is [`Ddl::CreateView`], which always replaces.
    ///
    /// `Ok(false)` means the name already held this statement and nothing was created.
    pub fn create_view(
        &self,
        view: &str,
        text: &str,
        or_replace: bool,
        if_not_exists: bool,
    ) -> Result<bool> {
        let existing = self.api.views().into_iter().find(|v| v.qualified() == view);
        if let Some(existing) = existing {
            if if_not_exists || existing.text == text {
                return Ok(false);
            }
            if !or_replace {
                return Err(ClusterError::Local(big_embed::ApiError::Db(
                    big_db::DbError::ViewRedefined(view.to_string()),
                )));
            }
        }
        self.ddl(&Ddl::CreateView { view: view.to_string(), text: text.to_string() })
            .map(|n| n == 1)
    }

    /// Forgets a view everywhere. `Ok(false)` means there was no such view.
    pub fn drop_view(&self, view: &str) -> Result<bool> {
        self.ddl(&Ddl::DropView { view: view.to_string() }).map(|n| n == 1)
    }

    /// `Ok(false)` means there was no such table - at the leader, which is the node whose
    /// answer is the cluster's answer.
    pub fn drop_table(&self, table: &str) -> Result<bool> {
        self.ddl(&Ddl::DropTable { table: table.to_string() }).map(|n| n == 1)
    }

    /// `TRUNCATE TABLE`: empties a table everywhere, keeping it declared.
    ///
    /// Travels the same leader-then-fan-out path a drop takes, and for the sharper version of
    /// the same reason: a node that missed it still holds the records, and unlike a drop there
    /// is no name missing afterwards to make that visible. What comes back is the leader's
    /// count of fragments, which is what every other schema change answers with.
    pub fn truncate_table(&self, table: &str) -> Result<u64> {
        self.ddl(&Ddl::TruncateTable { table: table.to_string() })
    }

    /// Retention, cluster-wide: drop a time-quantum field's index before an instant.
    ///
    /// **This is the spelling `big-bin`'s offline tool said belonged with the other DDL.** Its
    /// own doc gives the reason: a node that dropped a day while another has not would answer a
    /// window query with part of the truth and no symptom at all. Going through the leader and
    /// then every node is what makes the answer the same everywhere.
    pub fn drop_views_before(&self, table: &str, field: &str, unix_seconds: i64) -> Result<u64> {
        self.ddl(&Ddl::DropViewsBefore {
            table: table.to_string(),
            field: field.to_string(),
            unix_seconds,
        })
    }

    pub fn drop_field(&self, table: &str, field: &str) -> Result<bool> {
        self.ddl(&Ddl::DropField { table: table.to_string(), field: field.to_string() })
            .map(|n| n == 1)
    }

    /// The leader first, then everybody else.
    ///
    /// The leader decides: a name that is already taken, a field kind that contradicts an
    /// existing one, a table that is not there to drop - all of that is refused in one place,
    /// before any other node has heard of it. What the other nodes do is apply a change that
    /// has already been ruled legal.
    ///
    /// The number that comes back is the leader's. Table and field ids are a node's own
    /// numbering and nothing on the wire depends on two nodes agreeing about them; names do
    /// the resolving, all the way down. Reporting one node's is a choice about which of two
    /// equally true numbers to print, and the leader's is the one that was assigned first.
    pub(super) fn ddl(&self, op: &Ddl) -> Result<u64> {
        let leader = self.schema_leader();
        let body = op.encode();
        let answer = if self.leads_schema() {
            apply_ddl(&self.api, op).map_err(ClusterError::Local)?
        } else {
            let bytes = self.ask(leader, path::DDL, &body, None).map_err(|e| match e {
                ClusterError::Unreachable { node, why, .. } => {
                    ClusterError::LeaderUnreachable { node, why }
                }
                other => other,
            })?;
            wire::get_u64_body(&bytes).map_err(|why| ClusterError::Wire {
                node: self.describe(self.schema_leader()),
                why,
            })?
        };

        let rest: Vec<usize> = (0..self.config.nodes().len()).filter(|i| *i != leader).collect();
        if rest.is_empty() {
            return Ok(answer);
        }

        let mut committed = vec![self.describe(leader)];
        let mut failed = Vec::new();
        for i in rest {
            let outcome = if i == self.config.this_index() {
                apply_ddl(&self.api, op).map(|_| ()).map_err(ClusterError::Local)
            } else {
                self.ask(i, path::DDL, &body, None).map(|_| ())
            };
            match outcome {
                Ok(()) => committed.push(self.describe(i)),
                Err(e) => failed.push(format!("{} ({e})", self.describe(i))),
            }
        }

        if failed.is_empty() {
            Ok(answer)
        } else {
            // Every node is attempted even after the first failure, unlike a batch of facts:
            // there is nothing to lose by trying the rest, and a schema change that reached
            // three nodes out of four is finished by hand from a list of the one that is left.
            Err(ClusterError::Partial { what: "the schema change", committed, failed })
        }
    }
}

/// The change that creates one field exactly as another node has it.
pub(super) fn create_field(table: &str, field: &big_embed::FieldInfo) -> Ddl {
    match field.kind {
        big_embed::FieldKind::Decimal => Ddl::CreateDecimal {
            table: table.to_string(),
            field: field.name.clone(),
            bit_depth: field.bit_depth,
            scale: field.scale,
        },
        big_embed::FieldKind::TimeQuantum => Ddl::CreateTimeQuantum {
            table: table.to_string(),
            field: field.name.clone(),
            granularity: field.granularity.clone(),
        },
        kind => Ddl::CreateField {
            table: table.to_string(),
            field: field.name.clone(),
            kind,
            bit_depth: field.bit_depth,
        },
    }
}

/// Applies one schema change to one database.
///
/// The same function on both sides of the wire: a coordinator calls it for its own node and
/// the `/internal/ddl` handler calls it for a peer's. Two implementations would be two answers
/// to what a schema change means.
pub fn apply_ddl<P: PagerMut + Sync>(api: &Api<P>, op: &Ddl) -> big_embed::Result<u64> {
    Ok(match op {
        Ddl::CreateTable { table, engine } => api.create_table_with(table, *engine)? as u64,
        Ddl::CreateField { table, field, kind, bit_depth } => {
            api.create_field(table, field, *kind, *bit_depth)? as u64
        }
        Ddl::CreateDecimal { table, field, bit_depth, scale } => {
            api.create_decimal(table, field, *bit_depth, *scale)? as u64
        }
        Ddl::CreateTimeQuantum { table, field, granularity } => {
            api.create_time_quantum(table, field, granularity.clone())? as u64
        }
        Ddl::DropTable { table } => api.drop_table(table)? as u64,
        // A table that is not here counts as nothing truncated rather than as a failure: the
        // leader has already ruled on whether it exists, so a peer without it is a peer that
        // never had the data this is removing.
        Ddl::TruncateTable { table } => api.truncate_table(table)?.unwrap_or(0),
        Ddl::DropViewsBefore { table, field, unix_seconds } => {
            api.drop_days_before(table, field, *unix_seconds)? as u64
        }
        Ddl::DropField { table, field } => api.drop_field(table, field)? as u64,
        Ddl::CreateDatabase { name } => api.create_database(name)? as u64,
        // Always cascading here: whether `CASCADE` was written was judged at the leader, and
        // what reaches a peer is a change already ruled legal. A peer re-deciding it against
        // its own table count would be a second opinion, and the two could differ.
        Ddl::DropDatabase { name } => api.drop_database(name, true)? as u64,
        // Always the replacing form, for the reason above: whether `OR REPLACE` was written was
        // judged at the leader against the definition that was there, and a peer re-deciding it
        // could refuse a change the leader already made.
        Ddl::CreateView { view, text } => api.create_view(view, text, true)? as u64,
        Ddl::DropView { view } => api.drop_view(view)? as u64,
        Ddl::CreateRole { role } => api.create_role(role)? as u64,
        Ddl::DropRole { role } => api.drop_role(role)? as u64,
        // The mask is applied as it arrives, for the reason `CreateView` is always the replacing
        // form: whether it was a `GRANT` or a `REVOKE`, and what was there before, was judged at
        // the leader. A peer recomputing it against its own grants would be a second opinion.
        Ddl::SetGrant { role, database, table, privileges } => {
            let database = (!database.is_empty()).then_some(database.as_str());
            let table = (!table.is_empty()).then_some(table.as_str());
            api.set_grant(role, database, table, big_rbac::Privileges(*privileges))?;
            1
        }
    })
}
