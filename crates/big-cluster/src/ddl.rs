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
    pub fn schema(&self) -> Vec<big_api::TableInfo> {
        self.api.schema()
    }

    /// Creates a table under the default engine.
    pub fn create_table(&self, table: &str) -> Result<u64> {
        self.create_table_with(table, big_api::TableEngine::default())
    }

    /// The same, with the storage engine named.
    ///
    /// The engine is part of the change rather than each node's own decision: two copies of one
    /// table that stored different things would answer the same query at different costs, and a
    /// failover would change a query's cost without changing the query.
    pub fn create_table_with(&self, table: &str, engine: big_api::TableEngine) -> Result<u64> {
        self.ddl(&Ddl::CreateTable { table: table.to_string(), engine })
    }

    pub fn create_field(
        &self,
        table: &str,
        field: &str,
        kind: big_api::FieldKind,
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
        granularity: Vec<big_api::Granularity>,
    ) -> Result<u64> {
        self.ddl(&Ddl::CreateTimeQuantum {
            table: table.to_string(),
            field: field.to_string(),
            granularity,
        })
    }

    /// `Ok(false)` means there was no such table - at the leader, which is the node whose
    /// answer is the cluster's answer.
    pub fn drop_table(&self, table: &str) -> Result<bool> {
        self.ddl(&Ddl::DropTable { table: table.to_string() }).map(|n| n == 1)
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
        let leader = self.config.leader_index();
        let body = op.encode();
        let answer = if self.config.leads_schema() {
            apply_ddl(&self.api, op).map_err(ClusterError::Local)?
        } else {
            let bytes = self.ask(leader, path::DDL, &body, None).map_err(|e| match e {
                ClusterError::Unreachable { node, why, .. } => {
                    ClusterError::LeaderUnreachable { node, why }
                }
                other => other,
            })?;
            wire::get_u64_body(&bytes).map_err(|why| ClusterError::Wire {
                node: self.config.leader().name.clone(),
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
pub(super) fn create_field(table: &str, field: &big_api::FieldInfo) -> Ddl {
    match field.kind {
        big_api::FieldKind::Decimal => Ddl::CreateDecimal {
            table: table.to_string(),
            field: field.name.clone(),
            bit_depth: field.bit_depth,
            scale: field.scale,
        },
        big_api::FieldKind::TimeQuantum => Ddl::CreateTimeQuantum {
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
pub fn apply_ddl<P: PagerMut + Sync>(api: &Api<P>, op: &Ddl) -> big_api::Result<u64> {
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
        Ddl::DropField { table, field } => api.drop_field(table, field)? as u64,
    })
}
