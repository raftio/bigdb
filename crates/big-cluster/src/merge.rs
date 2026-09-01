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

//! Putting the owners' answers back together.
//!
//! **The merge is directed by the plan, not by the answers.** `Value::Extreme` does not say
//! whether it came from a `Min` or a `Max`, and folding two of them the wrong way is a wrong
//! answer that looks like a right one. The plan is the only thing that knows, and the
//! coordinator has it because it made it.
//!
//! **Groups are summed before they are ranked or cut.** Each of `Distinct`, `TopN` and
//! `GroupBy` is a per-row aggregate over groups, so a row that is merely second on every node
//! would lose to one that leads a single node if a node's list were truncated on its way here.
//! `TopN` therefore asks every owner for *all* of its groups and cuts the list once, at the
//! end. A tighter bound is possible and is an optimisation, not a correction.
//!
//! The ordering itself is [`big_exec::sort_by_key`] and [`big_exec::rank_top_n`], reached for
//! rather than reimplemented: two definitions of one order would mean the same query answered
//! two ways depending on how many nodes were asked.

use crate::error::{ClusterError, Result};
use big_api::{Plan, RowId, Value};
use big_exec::Group;
use std::collections::BTreeMap;

/// Folds owners' answers into one, in whatever order they arrive.
///
/// Four of the seven shapes are a running total and need nothing kept; the three that group
/// keep a map until the end, because ranking cannot begin before the last contribution.
pub struct Merge<'a> {
    plan: &'a Plan,
    acc: Option<Value>,
    /// Set only for the grouping plans. Kept beside `acc` rather than inside it so that a
    /// half-merged group list is never mistakable for an answer.
    groups: Option<BTreeMap<RowId, Group>>,
}

impl<'a> Merge<'a> {
    pub fn new(plan: &'a Plan) -> Self {
        let groups =
            matches!(plan, Plan::Distinct { .. } | Plan::TopN { .. } | Plan::GroupBy { .. })
                .then(BTreeMap::new);
        Self { plan, acc: None, groups }
    }

    /// One owner's answer.
    pub fn add(&mut self, node: &str, value: Value) -> Result<()> {
        if let Some(groups) = &mut self.groups {
            let Value::Groups(incoming) = value else {
                return Err(ClusterError::Mismatch { node: node.to_string(), what: "no groups" });
            };
            let aggregate = group_aggregate(self.plan);
            for g in incoming {
                match groups.get_mut(&g.row) {
                    Some(held) => {
                        let merged = combine(aggregate, node, (*held.value).clone(), *g.value)?;
                        *held.value = merged;
                        // A node that holds the row but not its name cannot exist today - a
                        // key reaches a node through the assignment that names it - but the
                        // shape allows one, and taking whichever name exists costs nothing.
                        if held.key.is_none() {
                            held.key = g.key;
                        }
                    }
                    None => {
                        groups.insert(g.row, g);
                    }
                }
            }
            return Ok(());
        }

        self.acc = Some(match self.acc.take() {
            None => value,
            Some(held) => combine(self.plan, node, held, value)?,
        });
        Ok(())
    }

    /// The answer, ordered and cut the way a single node would have produced it.
    ///
    /// An empty fold is only reachable with no owners at all, which the configuration refuses
    /// at startup; `Count(0)` is nevertheless the honest answer to it rather than a panic.
    pub fn finish(self) -> Value {
        if let Some(groups) = self.groups {
            let mut groups: Vec<Group> = groups.into_values().collect();
            match self.plan {
                Plan::TopN { n, .. } => big_exec::rank_top_n(&mut groups, *n),
                _ => big_exec::sort_by_key(&mut groups),
            }
            return Value::Groups(groups);
        }
        self.acc.unwrap_or(Value::Count(0))
    }
}

/// Two nodes' pair groupings, folded on the pair of rows.
fn merge_pairs(
    aggregate: &Plan,
    node: &str,
    a: Vec<big_api::Pair>,
    b: Vec<big_api::Pair>,
) -> Result<Vec<big_api::Pair>> {
    let mut held: BTreeMap<(RowId, RowId), big_api::Pair> = BTreeMap::new();
    for p in a.into_iter().chain(b) {
        match held.get_mut(&(p.left.row, p.right.row)) {
            None => {
                held.insert((p.left.row, p.right.row), p);
            }
            Some(there) => {
                let merged =
                    combine(aggregate, node, (*there.right.value).clone(), *p.right.value)?;
                *there.right.value = merged;
                // A node that holds the row but not its name cannot exist today; taking
                // whichever name exists costs nothing and keeps the shape honest.
                for (into, from) in
                    [(&mut there.left.key, p.left.key), (&mut there.right.key, p.right.key)]
                {
                    if into.is_none() {
                        *into = from;
                    }
                }
            }
        }
    }
    let mut out: Vec<big_api::Pair> = held.into_values().collect();
    big_exec::sort_pairs(&mut out);
    Ok(out)
}

/// Which plan decides how one group's measurement combines with another's.
///
/// `Distinct` and `TopN` measure with a count, which they do not spell out anywhere in the
/// plan; `GroupBy` carries the aggregate it was given. Standing in a `Count` for the first two
/// is what lets one function handle all three.
fn group_aggregate(plan: &Plan) -> &Plan {
    match plan {
        Plan::GroupBy { aggregate, .. } => aggregate,
        // Any `Count` will do: `combine` reads the variant, never the table or the rows.
        other => other,
    }
}

/// Two answers to the same plan, combined the way that plan's reduce combines.
fn combine(plan: &Plan, node: &str, a: Value, b: Value) -> Result<Value> {
    let wrong = |what: &'static str| ClusterError::Mismatch { node: node.to_string(), what };
    Ok(match (plan, a, b) {
        // Owners contribute disjoint shard sets, so a union cannot double-count. That is a
        // consequence of the ownership check at startup, not of anything here.
        (Plan::Rows { .. }, Value::Rows(x), Value::Rows(y)) => Value::Rows(x.or(&y)),

        // Each owner was asked for the whole page, because the first `limit` records overall
        // can all live on one node. The merge is what makes that page the right one: the two
        // lists interleave by record id and the cut happens once, here, after every owner has
        // contributed. A plan with no limit has no cut to make - every owner's whole answer is
        // the answer.
        (Plan::Project { limit, .. }, Value::Table(x), Value::Table(y)) => {
            Value::Table(big_exec::merge_projected(x, y, *limit))
        }

        // A pair holds records from one node or from several, so the two lists are folded
        // together on the pair of rows before anything is ordered. The same argument
        // `count(DISTINCT x)` makes one level up: a pair two nodes both hold is one pair.
        (Plan::GroupByPair { aggregate, .. }, Value::Pairs(x), Value::Pairs(y)) => {
            Value::Pairs(merge_pairs(aggregate, node, x, y)?)
        }

        (_, Value::Count(x), Value::Count(y)) => {
            Value::Count(x.checked_add(y).ok_or(ClusterError::Overflow)?)
        }
        (_, Value::Sum(x), Value::Sum(y)) => {
            Value::Sum(x.checked_add(y).ok_or(ClusterError::Overflow)?)
        }
        (_, Value::SignedSum(x), Value::SignedSum(y)) => {
            Value::SignedSum(x.checked_add(y).ok_or(ClusterError::Overflow)?)
        }

        // The extreme of the extremes. Absent stays absent: no records matched anywhere is not
        // the same answer as a total of nothing, and a node with nothing to say must not pull
        // a minimum down to zero.
        (Plan::Min { .. }, Value::Extreme(x), Value::Extreme(y)) => {
            Value::Extreme(pick(x, y, u64::min))
        }
        (Plan::Max { .. }, Value::Extreme(x), Value::Extreme(y)) => {
            Value::Extreme(pick(x, y, u64::max))
        }
        (Plan::Min { .. }, Value::SignedExtreme(x), Value::SignedExtreme(y)) => {
            Value::SignedExtreme(pick(x, y, i64::min))
        }
        (Plan::Max { .. }, Value::SignedExtreme(x), Value::SignedExtreme(y)) => {
            Value::SignedExtreme(pick(x, y, i64::max))
        }

        // Reached when a peer answers a shape this plan cannot produce, which means the two
        // nodes are not running the same build. Refused rather than folded into whichever
        // arm nearly fits.
        (_, Value::Rows(_), _) | (_, _, Value::Rows(_)) => return Err(wrong("a row set")),
        (_, Value::Groups(_), _) | (_, _, Value::Groups(_)) => return Err(wrong("groups")),
        (_, Value::Extreme(_) | Value::SignedExtreme(_), _) => {
            return Err(wrong("an extreme, for a query that is not Min or Max"))
        }
        _ => return Err(wrong("a differently shaped answer")),
    })
}

/// Neither, one, or the better of two.
fn pick<T: Copy>(a: Option<T>, b: Option<T>, better: impl Fn(T, T) -> T) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(better(a, b)),
        (only, None) | (None, only) => only,
    }
}

/// Owners' pages of record ids, merged into one ascending page.
///
/// Each owner answers from its own range and the ranges are disjoint, so this is a merge and
/// never a deduplication - two owners cannot both hold one record id. Cutting to `limit`
/// happens here rather than at the owners, for the same reason `TopN` is cut here: an owner
/// does not know what the others are holding.
pub fn merge_records(pages: Vec<Vec<u64>>, limit: usize) -> Vec<u64> {
    let mut heads: Vec<core::iter::Peekable<std::vec::IntoIter<u64>>> =
        pages.into_iter().map(|p| p.into_iter().peekable()).collect();
    let mut out = Vec::with_capacity(limit.min(1 << 16));
    while out.len() < limit {
        let next = heads
            .iter_mut()
            .enumerate()
            .filter_map(|(i, h)| h.peek().map(|v| (*v, i)))
            .min_by_key(|(v, _)| *v);
        let Some((_, i)) = next else { break };
        out.push(heads[i].next().expect("peek said there was one"));
    }
    out
}
