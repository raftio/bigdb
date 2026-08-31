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

//! Finding out whether the copies of a range agree, and making them agree again.
//!
//! `Cluster::verify` never fails as a whole: a node that cannot be reached is part of the
//! report rather than an error, because "which of these is not answering" is the question. It
//! is a scan - run it deliberately.

use super::*;

impl<P: PagerMut + Sync> Cluster<P> {
    /// Asks every copy of every range for a digest of what it holds, and reports the answers.
    ///
    /// **This is the price of replication without consensus, paid on demand.** A write goes to
    /// every copy and is reported half applied if one of them refuses, so the copies agree
    /// unless something went wrong that somebody was told about. A coordinator that died in
    /// the middle of a batch is the case where nobody was told, and this is how an operator
    /// finds out - before a failover rather than after one.
    ///
    /// Never fails as a whole. A node that cannot be reached is part of the report rather than
    /// an error, because "which of these is not answering" is exactly what was being asked.
    ///
    /// It is a scan. Run it deliberately, not every fifteen seconds.
    pub fn verify(&self) -> Vec<RangeVerdict> {
        let mut out = Vec::new();
        for range in 0..self.config.range_count() {
            let copies = self.copies_of_range(range);
            let primary = copies[0];
            let digests: Vec<CopyDigest> = std::thread::scope(|scope| {
                let handles: Vec<_> =
                    copies.iter().map(|&i| (i, scope.spawn(move || self.digest_of(i)))).collect();
                handles
                    .into_iter()
                    .map(|(i, h)| {
                        let node = self.config.nodes()[i].name.clone();
                        match h.join() {
                            Ok(Ok(d)) => CopyDigest { node, digest: Some(d), why: None },
                            Ok(Err(e)) => {
                                CopyDigest { node, digest: None, why: Some(e.to_string()) }
                            }
                            Err(_) => CopyDigest {
                                node,
                                digest: None,
                                why: Some("the request thread panicked".to_string()),
                            },
                        }
                    })
                    .collect()
            });

            // Unreachable is not agreement. A copy that did not answer is a copy nobody knows
            // anything about, and reporting that as agreement is the whole failure this exists
            // to prevent.
            let agree = digests.iter().all(|d| d.digest.is_some())
                && digests.windows(2).all(|w| w[0].digest == w[1].digest);
            out.push(RangeVerdict {
                shards: self.config.nodes()[primary].shards.to_string(),
                primary: self.config.nodes()[primary].name.clone(),
                copies: digests,
                agree,
            });
        }
        out
    }

    pub(super) fn digest_of(&self, node: usize) -> Result<u64> {
        if node == self.config.this_index() {
            return digest::digest(&self.api).map_err(ClusterError::Local);
        }
        let bytes = self.ask(node, path::DIGEST, &[], None)?;
        wire::get_u64_body(&bytes)
            .map_err(|why| ClusterError::Wire { node: self.config.nodes()[node].name.clone(), why })
    }

    // -----------------------------------------------------------------------------------
    // Catching a copy up
    // -----------------------------------------------------------------------------------

    /// Brings every copy the agreement has marked behind back into line with the copy serving
    /// its range, and clears the mark.
    ///
    /// **A copy, not a merge, and that is a consequence of choosing consistency.** Every write
    /// reaches the copy serving the range before it reaches any other, so that copy is the
    /// truth and this is not reconciling two opinions - it is replacing one. A merge would be
    /// wrong in the direction that matters: a copy that missed a *deletion* holds bits the
    /// truth does not, and a union would put them back.
    ///
    /// **What it moves is what differs.** Fragments are compared by cardinality first, and
    /// under the rule above that comparison is a proof rather than a hint: a copy that is
    /// behind holds a subset, and a subset with the same count is the same set. A blip that
    /// cost one batch therefore costs one fragment to repair, not a database.
    ///
    /// One fragment is one transaction. A repair that is interrupted leaves the copy closer to
    /// the truth than it was and still marked behind, which is exactly the state it should be
    /// left in.
    pub fn repair(&self) -> Result<Vec<RepairReport>> {
        let Some(controller) = &self.controller else { return Ok(Vec::new()) };
        let behind = controller.ownership().stale;
        let mut out = Vec::new();
        for node in behind {
            let Some(range) = self.config.range_of_node(node) else { continue };
            let source = self.serving(range);
            if source == node {
                // The copy that is behind is the one serving the range. Nothing here can fix
                // that: there is no more authoritative copy to take from.
                out.push(RepairReport {
                    node: self.config.nodes()[node].name.clone(),
                    fragments: 0,
                    outcome: "this copy is the one serving its range".to_string(),
                });
                continue;
            }
            let report = match self.catch_up(source, node) {
                Ok(fragments) => {
                    let cleared = self.clear_stale(node);
                    RepairReport {
                        node: self.config.nodes()[node].name.clone(),
                        fragments,
                        outcome: if cleared {
                            "caught up".to_string()
                        } else {
                            // The data moved and the mark did not, so the copy is correct and
                            // still will not be promoted. Saying so is the difference between
                            // a repair to run again and a repair to worry about.
                            "caught up, but the agreement did not record it".to_string()
                        },
                    }
                }
                Err(e) => RepairReport {
                    node: self.config.nodes()[node].name.clone(),
                    fragments: 0,
                    outcome: e.to_string(),
                },
            };
            out.push(report);
        }
        Ok(out)
    }

    /// Makes `target` hold what `source` holds. Returns how many fragments had to move.
    pub(super) fn catch_up(&self, source: usize, target: usize) -> Result<usize> {
        // The schema first, or nothing else can land: a fragment belongs to a field, and a
        // node that was away while the field was created has never heard of it.
        let mut moved = self.match_schema(source, target)?;
        for table in self.pull_schema(source)? {
            // The keys first. A fragment is rows of bits, and a row is a number until
            // something says which string it stands for.
            let keys = self.pull_keys(source, &table.name)?;
            self.push_keys(target, &table.name, keys)?;

            let mine = self.pull_fragments(source, &table.name)?;
            let theirs = self.pull_fragments(target, &table.name)?;
            for (addr, count) in &mine {
                let same = theirs.iter().any(|(a, c)| a == addr && c == count);
                if same {
                    continue;
                }
                let body = self.pull_fragment(source, addr)?;
                self.push_fragment(target, &body)?;
                moved += 1;
            }
            // Anything the target holds and the source does not is something the target should
            // not have: a fragment whose table or field was dropped while it was away, or one
            // emptied by a deletion it missed. Replaced with nothing, which frees its pages.
            for (addr, _) in &theirs {
                if mine.iter().any(|(a, _)| a == addr) {
                    continue;
                }
                self.push_fragment(
                    target,
                    &wire::FragmentBody {
                        addr: addr.clone(),
                        meta: big_api::FragmentMeta::default(),
                        // Emptied in whichever units this address stores, so that replacing a
                        // segment with "no containers" cannot be what frees it.
                        data: if addr.view.is_none() && addr.view_id == big_db::COLUMN_VIEW {
                            big_api::FragmentData::Cells(Vec::new())
                        } else {
                            big_api::FragmentData::Containers(Vec::new())
                        },
                    },
                )?;
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// Makes `target`'s schema the same as `source`'s, and says how many changes that took.
    ///
    /// Additive first and destructive second, so that a field being *replaced* - dropped and
    /// created with a different kind while this copy was away - ends up as the source has it
    /// rather than as a conflict.
    pub(super) fn match_schema(&self, source: usize, target: usize) -> Result<usize> {
        let mine = self.pull_schema(source)?;
        let theirs = self.pull_schema(target)?;
        let mut changes = 0;

        for table in &mine {
            let existing = theirs.iter().find(|t| t.name == table.name);
            if existing.is_none() {
                // The engine travels with the name. A copy that recreated the table under
                // the default would answer the same questions at a different cost, and a
                // repair is supposed to leave two nodes indistinguishable.
                self.tell(
                    target,
                    &Ddl::CreateTable { table: table.name.clone(), engine: table.engine },
                )?;
                changes += 1;
            }
            for field in &table.fields {
                let held = existing.and_then(|t| t.fields.iter().find(|f| f.name == field.name));
                match held {
                    // A field of the same name and a different shape is not the same field.
                    // Dropped rather than reconciled: its bits mean something else.
                    Some(h) if h.kind != field.kind || h.bit_depth != field.bit_depth => {
                        self.tell(
                            target,
                            &Ddl::DropField {
                                table: table.name.clone(),
                                field: field.name.clone(),
                            },
                        )?;
                        changes += 1;
                    }
                    Some(_) => continue,
                    None => {}
                }
                self.tell(target, &create_field(&table.name, field))?;
                changes += 1;
            }
        }

        for table in &theirs {
            let Some(kept) = mine.iter().find(|t| t.name == table.name) else {
                self.tell(target, &Ddl::DropTable { table: table.name.clone() })?;
                changes += 1;
                continue;
            };
            for field in &table.fields {
                if !kept.fields.iter().any(|f| f.name == field.name) {
                    self.tell(
                        target,
                        &Ddl::DropField { table: table.name.clone(), field: field.name.clone() },
                    )?;
                    changes += 1;
                }
            }
        }
        Ok(changes)
    }

    pub(super) fn pull_schema(&self, node: usize) -> Result<Vec<big_api::TableInfo>> {
        if node == self.config.this_index() {
            return Ok(self.api.schema());
        }
        let bytes = self.ask(node, path::SCHEMA, &[], None)?;
        self.read(node, || wire::get_schema(&bytes))
    }

    pub(super) fn pull_keys(&self, node: usize, table: &str) -> Result<Vec<Assignment>> {
        if node == self.config.this_index() {
            return Ok(self
                .api
                .row_keys(table)?
                .into_iter()
                .map(|(field, key, row)| Assignment { field, key, row })
                .collect());
        }
        let body = wire::FragmentsRequest { table: table.to_string() }.encode();
        let bytes = self.ask(node, path::KEYS, &body, None)?;
        Ok(self.read(node, || wire::KeysBody::decode(&bytes))?.keys)
    }

    pub(super) fn push_keys(&self, node: usize, table: &str, keys: Vec<Assignment>) -> Result<()> {
        if node == self.config.this_index() {
            let borrowed: Vec<KeyAssignment<'_>> = keys
                .iter()
                .map(|a| KeyAssignment { field: &a.field, key: &a.key, row: a.row })
                .collect();
            return self.api.assign_keys(table, &borrowed).map_err(ClusterError::Local);
        }
        let body = wire::KeysBody { table: table.to_string(), keys }.encode();
        self.ask(node, path::KEYS_PUT, &body, None).map(|_| ())
    }

    pub(super) fn pull_fragments(
        &self,
        node: usize,
        table: &str,
    ) -> Result<Vec<(big_api::FragmentAddr, u64)>> {
        if node == self.config.this_index() {
            return Ok(self.api.fragments(table)?);
        }
        let body = wire::FragmentsRequest { table: table.to_string() }.encode();
        let bytes = self.ask(node, path::FRAGMENTS, &body, None)?;
        self.read(node, || wire::get_fragment_list(&bytes))
    }

    pub(super) fn pull_fragment(
        &self,
        node: usize,
        addr: &big_api::FragmentAddr,
    ) -> Result<wire::FragmentBody> {
        if node == self.config.this_index() {
            let (meta, data) = self.api.fragment(addr)?;
            return Ok(wire::FragmentBody {
                addr: addr.clone(),
                meta: meta.unwrap_or_default(),
                data,
            });
        }
        let body = wire::FragmentRequest { addr: addr.clone() }.encode();
        let bytes = self.ask(node, path::FRAGMENT, &body, None)?;
        self.read(node, || wire::FragmentBody::decode(&bytes))
    }

    pub(super) fn push_fragment(&self, node: usize, body: &wire::FragmentBody) -> Result<()> {
        if node == self.config.this_index() {
            return self
                .api
                .replace_fragment(&body.addr, body.meta, &body.data)
                .map_err(ClusterError::Local);
        }
        self.ask(node, path::FRAGMENT_PUT, &body.encode(), None).map(|_| ())
    }

    /// Tells the agreement that a copy has caught up, wherever the agreement's leader is.
    pub(super) fn clear_stale(&self, node: usize) -> bool {
        let Some(controller) = &self.controller else { return false };
        if controller.is_leader() {
            return controller.mark_repaired(node);
        }
        let Some(leader) = controller.leader() else { return false };
        if leader == self.config.this_index() {
            return controller.mark_repaired(node);
        }
        self.ask(leader, path::REPAIRED, &wire::put_node(node), None).is_ok()
    }
}
