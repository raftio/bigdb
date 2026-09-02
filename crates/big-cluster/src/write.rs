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

//! Writes: resolve every key first, then split the batch by the owner of each record.
//!
//! The ordering is the design. Keys are resolved against the schema leader **before any fact
//! is sent anywhere**, so a key that cannot be interned fails a batch that has written nothing;
//! after that a batch spanning two owners is two commits and the error says which landed.
//!
//! Two owners never contend, because a record id belongs to exactly one of them.

use super::*;

impl<P: PagerMut + Sync> Cluster<P> {
    /// Writes a batch, splitting it by the owner of each record.
    ///
    /// Every key is resolved against the schema leader **before any fact is sent anywhere**, so
    /// a key that cannot be interned fails the whole batch while nothing has been written.
    /// After that there is no going back: a batch spanning two owners is two commits, and the
    /// error says which shards landed.
    ///
    /// Two owners never contend. A record id belongs to exactly one of them and a fragment
    /// holds facts about its own records only, so the single-writer constraint is per node and
    /// the cluster's write throughput is the sum.
    /// Whether this node writes a batch by itself, with no peer to route to.
    ///
    /// Public so a caller holding facts that borrow their request body can ask *before* copying
    /// them into [`OwnedFact`]s. On the single-node path that copy is pure waste - the very next
    /// line borrows them straight back - and on the import route it was two `String`
    /// allocations per fact, on the one route whose whole purpose is volume.
    pub fn writes_alone(&self) -> bool {
        self.config.owns_everything()
    }

    /// [`Cluster::import`] for a caller whose facts are already borrowed.
    ///
    /// Only valid when [`Cluster::writes_alone`] is true, because a batch that has to be routed
    /// needs owned data to group by range and ship. Refused rather than silently mis-routed.
    pub fn import_borrowed(
        &self,
        table: &str,
        facts: &[big_embed::Fact<'_>],
    ) -> Result<WriteOutcome> {
        debug_assert!(self.writes_alone(), "import_borrowed on a node with peers");
        if !self.writes_alone() {
            return self.import(table, &facts.iter().map(OwnedFact::from_fact).collect::<Vec<_>>());
        }
        self.api.import(table, facts)?;
        Ok(WriteOutcome { count: facts.len() as u64, missed: Vec::new() })
    }

    pub fn import(&self, table: &str, facts: &[OwnedFact]) -> Result<WriteOutcome> {
        // One node owning everything is also the leader, so interning and writing are the same
        // transaction they have always been. Splitting them would pay for an agreement with
        // nobody to agree with: two commits, four fsyncs, for a batch one node writes alone.
        if self.config.owns_everything() {
            let borrowed: Vec<big_embed::Fact<'_>> = facts.iter().map(OwnedFact::as_fact).collect();
            self.api.import(table, &borrowed)?;
            return Ok(WriteOutcome { count: facts.len() as u64, missed: Vec::new() });
        }

        let assignments = self.resolve_keys(table, facts)?;
        // Grouped by *range*, not by node. Which node serves a range can move; which range a
        // record belongs to cannot, because it is a shift of the record id.
        let mut by_range: BTreeMap<usize, Vec<&OwnedFact>> = BTreeMap::new();
        for fact in facts {
            let range = self.config.range_of(big_engine::shard_of(fact.record));
            by_range.entry(range).or_default().push(fact);
        }

        let mut report = Report::new();
        // Sequential across ranges rather than concurrent. A commit is the one thing here that
        // cannot be taken back, so the failure this orders against is the interesting one: a
        // range that refuses stops the batch reaching the ranges after it, and the report says
        // exactly which ones landed.
        'ranges: for (range, share) in by_range {
            let keys = keys_used(&assignments, &share);
            // The copy currently serving the range first, then the rest. A write goes to every
            // copy or the copies stop agreeing, and the order is what decides which copy has
            // the batch when only one of them does - the one reads go to.
            let copies = self.copies_of_range(range);
            let primary = copies[0];
            for copy in copies {
                match self.write_share(copy, table, &keys, &share) {
                    Ok(()) => report.landed(self.describe(copy)),
                    Err(e) => match report.refused(self.describe(copy), e, copy == primary) {
                        Verdict::Stop => return Err(report.into_error("the batch")),
                        Verdict::AbandonRange => break 'ranges,
                        Verdict::CarryOn => {}
                    },
                }
            }
        }
        report.finish("the batch", facts.len() as u64)
    }

    /// One node's share of a batch, whether that node is this one or another.
    pub(super) fn write_share(
        &self,
        node: usize,
        table: &str,
        keys: &[Assignment],
        share: &[&OwnedFact],
    ) -> Result<()> {
        if node == self.config.this_index() {
            self.guard()?;
            let borrowed: Vec<KeyAssignment<'_>> = keys
                .iter()
                .map(|a| KeyAssignment { field: &a.field, key: &a.key, row: a.row })
                .collect();
            let facts: Vec<big_embed::Fact<'_>> =
                share.iter().map(|f| OwnedFact::as_fact(f)).collect();
            return self
                .api
                .import_with_keys(table, &borrowed, &facts)
                .map_err(ClusterError::Local);
        }
        let body = wire::ImportRequest {
            table: table.to_string(),
            keys: keys.to_vec(),
            facts: share.iter().map(|f| (*f).clone()).collect(),
        }
        .encode();
        self.ask(node, path::IMPORT, &body, None).map(|_| ())
    }

    /// Removes records from every field of a table, owner by owner.
    ///
    /// Same shape as [`Cluster::import`] and the same absence of atomicity across nodes, which
    /// matters less here only because a repeated delete is already safe: deleting a record that
    /// was never written is a request that was already satisfied.
    pub fn delete(&self, table: &str, records: &[RecordId]) -> Result<WriteOutcome> {
        let mut by_range: BTreeMap<usize, Vec<RecordId>> = BTreeMap::new();
        for record in records {
            let range = self.config.range_of(big_engine::shard_of(*record));
            by_range.entry(range).or_default().push(*record);
        }

        let mut removed = 0u64;
        let mut report = Report::new();
        'ranges: for (range, share) in by_range {
            let copies = self.copies_of_range(range);
            let primary = copies[0];
            for copy in copies {
                let outcome = if copy == self.config.this_index() {
                    self.guard()
                        .and_then(|()| self.api.delete(table, &share).map_err(ClusterError::Local))
                } else {
                    let body =
                        wire::DeleteRequest { table: table.to_string(), records: share.clone() }
                            .encode();
                    self.ask(copy, path::DELETE, &body, None).and_then(|bytes| {
                        wire::get_u64_body(&bytes).map_err(|why| ClusterError::Wire {
                            node: self.config.nodes()[copy].name.clone(),
                            why,
                        })
                    })
                };
                match outcome {
                    Ok(n) => {
                        // Only the copy that answers reads is counted. The others removed the
                        // same records, and counting them again would tell the caller it
                        // deleted each record once per copy.
                        if copy == primary {
                            removed += n;
                        }
                        report.landed(self.describe(copy));
                    }
                    Err(e) => match report.refused(self.describe(copy), e, copy == primary) {
                        Verdict::Stop => return Err(report.into_error("the deletion")),
                        Verdict::AbandonRange => break 'ranges,
                        Verdict::CarryOn => {}
                    },
                }
            }
        }
        report.finish("the deletion", removed)
    }

    /// What every key in this batch means, asking the schema leader about the ones this node
    /// does not already know.
    ///
    /// Row ids are immutable and never reused, so a mapping this node already holds can never
    /// be stale: a miss costs a round trip, never a wrong answer. A coordinator that owns none
    /// of the batch holds nothing to hit, which is the cost of a coordinator being any node
    /// rather than a tier.
    pub(super) fn resolve_keys(&self, table: &str, facts: &[OwnedFact]) -> Result<Vec<Assignment>> {
        let mut wanted: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for fact in facts {
            if let Some((field, key)) = fact.key() {
                wanted.entry(field).or_default().insert(key);
            }
        }

        let mut out = Vec::new();
        for (field, keys) in wanted {
            let mut misses: Vec<&str> = Vec::new();
            for key in keys {
                match self.api.key_row(table, field, key) {
                    Some(row) => {
                        out.push(Assignment { field: field.to_string(), key: key.to_string(), row })
                    }
                    None => misses.push(key),
                }
            }
            if misses.is_empty() {
                continue;
            }
            let rows = self.intern(table, field, &misses)?;
            if rows.len() != misses.len() {
                return Err(ClusterError::Mismatch {
                    node: self.config.leader().name.clone(),
                    what: "a different number of row ids than keys",
                });
            }
            for (key, row) in misses.into_iter().zip(rows) {
                out.push(Assignment { field: field.to_string(), key: key.to_string(), row });
            }
        }
        Ok(out)
    }

    /// A run of `count` record ids nobody else will be given.
    ///
    /// **Allocated by the schema leader, for the reason keys are interned there.** Two
    /// coordinators computing "one past the highest" would compute the same number and write
    /// two records into one - and unlike a refusal, nothing downstream could see that it had
    /// happened. So this goes through the one node whose job is to be the only one deciding,
    /// and a leader that cannot be reached stops the statement rather than guessing.
    ///
    /// Answers the first id of the run; the caller takes `count` consecutive ids from it.
    pub(super) fn allocate(&self, table: &str, count: u64) -> Result<RecordId> {
        if self.config.leads_schema() {
            return self.allocate_here(table, count);
        }
        let leader = self.config.leader_index();
        let body = wire::AllocateRequest { table: table.to_string(), count }.encode();
        let bytes = self.ask(leader, path::ALLOCATE, &body, None).map_err(|e| match e {
            ClusterError::Unreachable { node, why, .. } => {
                ClusterError::LeaderUnreachable { node, why }
            }
            other => other,
        })?;
        wire::get_u64_body(&bytes)
            .map_err(|why| ClusterError::Wire { node: self.config.leader().name.clone(), why })
    }

    /// The leader's own half of `Cluster::allocate`.
    ///
    /// Two terms, and both are needed. The first is one past the highest id **anywhere**, which
    /// is what keeps an allocation clear of ids written explicitly or through the import route -
    /// neither of which passes through here. The second is this leader's own floor, which keeps
    /// two allocations made before either has been committed from being handed the same number.
    ///
    /// Public because the peer route calls it: a request from another coordinator is the same
    /// allocation, made by the same node, for the same reason.
    pub fn allocate_here(&self, table: &str, count: u64) -> Result<RecordId> {
        // Held across the fan-out on purpose. It is one round trip per statement, and a lock
        // released before the floor was raised would be a lock that decided nothing.
        let mut floor = self.allocated.lock().unwrap_or_else(|e| e.into_inner());
        let anywhere = self.next_record(table)?;
        let from = anywhere.max(floor.get(table).copied().unwrap_or(0));
        floor.insert(table.to_string(), from.saturating_add(count));
        Ok(from)
    }

    /// One past the highest record id any node holds for a table.
    ///
    /// One shard's work per node - see `big_embed::Api::max_record` - and one round trip per
    /// statement that allocates, rather than per row. Zero for a table nobody has written to.
    pub(super) fn next_record(&self, table: &str) -> Result<RecordId> {
        let body = wire::TableRequest { table: table.to_string() }.encode();
        let asked = self.candidates(0..self.config.range_count());
        let answers =
            self.fan_out_over(&asked, None, path::NEXT_RECORD, &body, wire::get_u64_body, || {
                self.guard()?;
                self.local_next_record(table)
            })?;
        Ok(answers.into_iter().map(|(_, next)| next).max().unwrap_or(0))
    }

    /// This node's share of that answer, which the peer route answers with.
    pub fn local_next_record(&self, table: &str) -> Result<RecordId> {
        let max = self.api.max_record(table).map_err(ClusterError::Local)?;
        Ok(max.map_or(0, |m| m.saturating_add(1)))
    }

    /// Asks the schema leader what these keys mean, assigning ids to the ones it has not seen.
    ///
    /// When the leader cannot be reached this is where a write introducing a new key stops.
    /// Not queued, not assigned locally and reconciled later: two row ids for one string is a
    /// silently wrong answer, and a refusal is not.
    pub(super) fn intern(&self, table: &str, field: &str, keys: &[&str]) -> Result<Vec<RowId>> {
        if self.config.leads_schema() {
            return Ok(self.api.intern_keys(table, field, keys)?);
        }
        let leader = self.config.leader_index();
        let body = wire::InternRequest {
            table: table.to_string(),
            field: field.to_string(),
            keys: keys.iter().map(|k| (*k).to_string()).collect(),
        }
        .encode();
        let bytes = self.ask(leader, path::INTERN, &body, None).map_err(|e| match e {
            // Told apart from an owner being unreachable on purpose: they fail differently
            // and an operator's next move is different for each.
            ClusterError::Unreachable { node, why, .. } => {
                ClusterError::LeaderUnreachable { node, why }
            }
            other => other,
        })?;
        wire::get_rows_ids(&bytes)
            .map_err(|why| ClusterError::Wire { node: self.config.leader().name.clone(), why })
    }
}

/// What a write has managed so far, and what to do about the next thing that refuses.
///
/// The interesting decision is in one place: **a copy that could not be reached does not fail
/// the write.** Failing it would mean a replica dying takes the whole range's *writes* down -
/// a hole automatic failover does not plug, because failover replaces a dead primary and this
/// is a dead spare. What happens instead is that the copy is named in the answer and the
/// agreement marks it behind, and a copy marked behind is one the agreement will not promote
/// until it has been repaired.
///
/// That is what makes this safe rather than convenient. Nothing ever reads from a copy that
/// is behind, because reads go to the copy serving the range and a copy that is behind cannot
/// become that copy.
struct Report {
    landed: Vec<String>,
    missed: Vec<String>,
    /// The first failure, kept so that a write which achieved nothing fails with the reason
    /// rather than with a summary of the reason.
    first: Option<ClusterError>,
    /// Whether anything failed for a reason that is not "that node did not answer".
    ///
    /// Tracked separately from `first` because the two questions differ: the *reason to
    /// report* is the earliest failure, and the *decision to make* depends on whether any of
    /// them was a refusal. A batch whose first problem was an unreachable spare and whose
    /// second was a schema disagreement is not a batch that quietly succeeded.
    refused: bool,
}

enum Verdict {
    /// Nothing has been written anywhere: fail cleanly, and the caller can send it again.
    Stop,
    /// This range has the batch nowhere. Writing more of it would leave a wider hole.
    AbandonRange,
    /// A copy is behind and the copy that answers reads is not. Keep going.
    CarryOn,
}

impl Report {
    fn new() -> Self {
        Self { landed: Vec::new(), missed: Vec::new(), first: None, refused: false }
    }

    fn landed(&mut self, node: String) {
        self.landed.push(node);
    }

    fn refused(&mut self, node: String, e: ClusterError, was_serving: bool) -> Verdict {
        let unreachable = e.is_unreachable();
        self.refused |= !unreachable;
        self.missed.push(format!("{node} ({e})"));
        if self.first.is_none() {
            self.first = Some(e);
        }
        if self.landed.is_empty() {
            return Verdict::Stop;
        }
        if was_serving {
            return Verdict::AbandonRange;
        }
        // A spare that could not be reached is a spare that is behind, which is a state the
        // agreement records and a repair clears. A spare that *refused* is something else -
        // a schema that disagrees, a key conflict - and carrying on past it would be writing
        // more of a batch that is already wrong somewhere.
        if unreachable {
            Verdict::CarryOn
        } else {
            Verdict::AbandonRange
        }
    }

    fn into_error(mut self, what: &'static str) -> ClusterError {
        self.first.take().unwrap_or(ClusterError::Partial {
            what,
            committed: self.landed,
            failed: self.missed,
        })
    }

    fn finish(self, what: &'static str, count: u64) -> Result<WriteOutcome> {
        if self.missed.is_empty() {
            return Ok(WriteOutcome { count, missed: Vec::new() });
        }
        // Every copy that could not be reached, and nothing that refused: the write stands and
        // the answer says which copies are behind, because a caller that is not told cannot
        // ask for a repair.
        if !self.refused && !self.landed.is_empty() {
            return Ok(WriteOutcome { count, missed: self.missed });
        }
        Err(ClusterError::Partial { what, committed: self.landed, failed: self.missed })
    }
}

/// The assignments one owner's share of a batch actually uses.
///
/// Sending every key to every owner would work and would make the batch quadratic in the
/// number of distinct keys: an owner does not need to be told what a key it never writes means.
fn keys_used(assignments: &[Assignment], share: &[&OwnedFact]) -> Vec<Assignment> {
    let used: BTreeSet<(&str, &str)> = share.iter().filter_map(|f| f.key()).collect();
    assignments
        .iter()
        .filter(|a| used.contains(&(a.field.as_str(), a.key.as_str())))
        .cloned()
        .collect()
}
