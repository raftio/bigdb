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

//! One number per node, for asking whether two copies of a range still agree.
//!
//! **This is the price of replication without consensus.** A write goes to every copy and is
//! reported half applied if one of them refuses, so the copies agree unless something went
//! wrong that somebody was told about. "Unless something went wrong" is not a guarantee, and
//! this is what turns it into a question an operator can actually ask: run it, compare, and
//! know rather than assume.
//!
//! **It is a comparison, not a proof.** Two databases with the same digest are overwhelmingly
//! likely to hold the same facts, and nothing here rules out the pair that does not. What it
//! does rule out is the failure that actually happens: a batch that reached one copy and not
//! the other, which changes a count and therefore changes this.
//!
//! **It is a scan.** Every keyed field is grouped and every integer field is summed, over every
//! record the node holds. This is an operator's tool run deliberately, not a probe run every
//! fifteen seconds.
//!
//! The digest is built out of *logical* answers - counts, row ids, sums - and never out of
//! bytes on disk. Two copies that hold the same facts have different files: pages land in
//! different places, the freelist differs, compaction may have run on one. A digest over the
//! file would report a difference on every pair of healthy nodes, which is the same as
//! reporting nothing.

use big_api::{Api, FieldKind, PagerMut, Plan, QueryOptions, Rows, Value};

/// Every fact this node holds, as one number.
///
/// Tables in name order, fields in name order, so that two nodes that were told about their
/// schema in a different order still produce the same digest. Nothing here depends on a table
/// or field *id*, which is each node's own numbering.
pub fn digest<P: PagerMut + Sync>(api: &Api<P>) -> big_api::Result<u64> {
    let mut h = Fnv::new();
    let mut tables = api.schema();
    tables.sort_by(|a, b| a.name.cmp(&b.name));

    for table in &tables {
        h.str(&table.name);
        h.u64(count(api, &table.name)?);

        let mut fields = table.fields.clone();
        fields.sort_by(|a, b| a.name.cmp(&b.name));
        for field in &fields {
            h.str(&field.name);
            h.u64(field.kind as u64);
            match field.kind {
                // Every row of the field, with how many records are in it. This is the part
                // that catches a missing batch: a fact that did not land is a row whose count
                // is one lower, on one copy only.
                FieldKind::Set | FieldKind::Mutex | FieldKind::TimeQuantum => {
                    let groups = api.execute(
                        &Plan::Distinct {
                            table: table.name.clone(),
                            rows: Rows::All,
                            field: field.name.clone(),
                        },
                        &QueryOptions::default(),
                    )?;
                    // Sorted by row rather than by key, because a row id means the same thing
                    // on every node and is cheaper to compare than the string it came from.
                    if let Value::Groups(mut g) = groups {
                        g.sort_by_key(|g| g.row);
                        for group in g {
                            h.u64(group.row);
                            h.u64(match *group.value {
                                Value::Count(n) => n,
                                _ => 0,
                            });
                        }
                    }
                }
                // A total rather than every value: a bit-sliced field has no cheap way to
                // enumerate itself, and a sum changes whenever a record it covers does.
                FieldKind::Int | FieldKind::Decimal | FieldKind::SignedInt => {
                    let total = api.execute(
                        &Plan::Sum {
                            table: table.name.clone(),
                            rows: Rows::All,
                            field: field.name.clone(),
                        },
                        &QueryOptions::default(),
                    )?;
                    match total {
                        Value::Sum(n) => h.u128(n),
                        Value::SignedSum(n) => h.u128(n as u128),
                        _ => {}
                    }
                }
                // Two rows, and both of them matter: "false" and "absent" are different
                // answers, so a digest that only counted the true row would miss half of a
                // missing write.
                FieldKind::Bool => {
                    for value in [true, false] {
                        let n = api.execute(
                            &Plan::Count {
                                table: table.name.clone(),
                                rows: Rows::Bool { field: field.name.clone(), value },
                            },
                            &QueryOptions::default(),
                        )?;
                        h.u64(n.as_count().unwrap_or(0));
                    }
                }
            }
        }
    }
    Ok(h.finish())
}

fn count<P: PagerMut + Sync>(api: &Api<P>, table: &str) -> big_api::Result<u64> {
    let plan = Plan::Count { table: table.to_string(), rows: Rows::All };
    Ok(api.execute(&plan, &QueryOptions::default())?.as_count().unwrap_or(0))
}

/// FNV-1a, 64 bit.
///
/// Not a cryptographic hash and does not need to be: what it is up against is a node that is
/// missing a batch, not a node that is lying about one. A node that would forge a digest is a
/// node that could equally forge the answer to any query, and no hash chosen here helps with
/// that.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn byte(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(0x100_0000_01b3);
    }

    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }

    fn u128(&mut self, v: u128) {
        for b in v.to_le_bytes() {
            self.byte(b);
        }
    }

    /// Length first, so that `["ab", "c"]` and `["a", "bc"]` are different digests.
    fn str(&mut self, s: &str) {
        self.u64(s.len() as u64);
        for b in s.as_bytes() {
            self.byte(*b);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}
