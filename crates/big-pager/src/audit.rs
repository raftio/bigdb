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

//! Accounting for every page in the file: reachable, free, or neither.
//!
//! **Nothing here frees anything.** It answers one question - *how many pages does this file
//! contain that nothing points at and nothing has recorded as free* - and answers it as a
//! number, not as an action. A page like that is invisible to every other measurement: it is
//! counted in `page_count`, so the file is that much bigger, and it is in no freelist run, so
//! it is never handed out again. It is also enough, on its own, to stop
//! [`crate::Store::truncate_tail`] releasing anything at all - that walks down from the end of
//! the file and stops at the first page that is not a reusable free run, so one orphan at the
//! top pins every free page beneath it.
//!
//! **The complement is only as good as the mark.** A page class the mark phase forgets is a
//! page class this reports as leaked, so the enumeration in [`crate::Store::begin_audit`] is
//! the whole correctness argument, and the class that is easiest to forget is the one no other
//! walk in this tree visits: the trees an old *snapshot* still names. `Db::copy_to` skips them
//! deliberately, because it is building a new history; an audit is asserting what the existing
//! history still needs, so the one thing that walk drops is the one thing this must add.

use crate::freelist::FreeRun;
use big_page::{Pgno, TxnId};

/// One bit per page.
///
/// **A bitset rather than a count, and that is not an optimisation.** Under copy-on-write a
/// snapshot's tree shares almost every page with the current one, so the same page is
/// legitimately reached many times over across the root set - a counter would multiply
/// reachability and report a negative number of leaks. [`big_btree::visit_tree`] does not
/// deduplicate either: a page reached through two cells is yielded twice. Marking an
/// already-marked bit is a no-op, so neither matters here.
///
/// [`big_btree::visit_tree`]: https://docs.rs/big-btree
#[derive(Clone, Debug)]
pub struct PageSet {
    words: Vec<u64>,
    len: u64,
}

impl PageSet {
    /// A set that can hold every page below `pages`, all of them clear.
    pub fn with_pages(pages: u64) -> Self {
        Self { words: vec![0; pages.div_ceil(64) as usize], len: pages }
    }

    /// How many pages this set is able to describe. Anything at or above it is out of the
    /// question being asked, not a member that happens to be absent.
    pub fn capacity(&self) -> u64 {
        self.len
    }

    /// Marks a page. `false` when the page is outside this set, which a caller counts rather
    /// than ignores: a reference past the end of the file is a fact worth reporting.
    pub fn insert(&mut self, pgno: Pgno) -> bool {
        let at = pgno as u64;
        if at >= self.len {
            return false;
        }
        self.words[(at / 64) as usize] |= 1 << (at % 64);
        true
    }

    pub fn contains(&self, pgno: Pgno) -> bool {
        let at = pgno as u64;
        at < self.len && self.words[(at / 64) as usize] & (1 << (at % 64)) != 0
    }

    /// How many pages are marked.
    pub fn count(&self) -> u64 {
        self.masked_words().map(|w| w.count_ones() as u64).sum()
    }

    /// Pages in neither this set nor `other`.
    pub fn count_absent_from_both(&self, other: &PageSet) -> u64 {
        // `& mask` on the complement, not on the inputs. Inverting a word turns every bit past
        // the end of the file into a page that is absent from both sets, and a file whose page
        // count is not a multiple of 64 would report up to 63 leaks that are not pages.
        self.pair(other).map(|(a, b, mask)| ((!(a | b)) & mask).count_ones() as u64).sum()
    }

    /// Every page in neither set, lowest first. `DoubleEndedIterator`, so the *highest* - the
    /// one that pins the tail - is one `next_back` away.
    pub fn absent_from_both<'a>(
        &'a self,
        other: &'a PageSet,
    ) -> impl DoubleEndedIterator<Item = Pgno> + 'a {
        let pages: Vec<Pgno> = self
            .pair(other)
            .enumerate()
            .flat_map(|(i, (a, b, mask))| {
                let free = (!(a | b)) & mask;
                (0..64u64).filter_map(move |bit| {
                    (free & (1 << bit) != 0).then_some((i as u64 * 64 + bit) as Pgno)
                })
            })
            .collect();
        pages.into_iter()
    }

    /// Pages in both sets, lowest first.
    pub fn present_in_both<'a>(&'a self, other: &'a PageSet) -> impl Iterator<Item = Pgno> + 'a {
        self.pair(other).enumerate().flat_map(|(i, (a, b, mask))| {
            let both = a & b & mask;
            (0..64u64).filter_map(move |bit| {
                (both & (1 << bit) != 0).then_some((i as u64 * 64 + bit) as Pgno)
            })
        })
    }

    /// The words of both sets, each with the mask of the pages that actually exist.
    ///
    /// The mask is handed out rather than pre-applied because the caller that needs it most is
    /// the one taking a *complement*: masking the inputs does nothing for `!(a | b)`, which
    /// sets every bit past the end of the file. A file whose page count is not a multiple of
    /// 64 would then report up to 63 leaks that are not pages at all.
    fn pair<'a>(&'a self, other: &'a PageSet) -> impl Iterator<Item = (u64, u64, u64)> + 'a {
        debug_assert_eq!(self.len, other.len, "two page sets over different files");
        let masks = self.masks();
        self.words.iter().zip(other.words.iter()).zip(masks).map(|((a, b), m)| (*a, *b, m))
    }

    /// One mask per word: all ones, except the last, which keeps only the pages that exist.
    fn masks(&self) -> impl Iterator<Item = u64> + '_ {
        let last = (self.len % 64) as u32;
        let tail = self.words.len().saturating_sub(1);
        (0..self.words.len()).map(move |i| {
            if i == tail && last != 0 {
                (1u64 << last) - 1
            } else {
                u64::MAX
            }
        })
    }

    fn masked_words(&self) -> impl Iterator<Item = u64> + '_ {
        self.words.iter().zip(self.masks()).map(|(w, m)| w & m)
    }
}

/// How many pages of each kind the mark phase reached. Every figure counts *marks made*, so a
/// page shared between a snapshot and the current tree is counted under both.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct AuditTally {
    pub meta: u64,
    pub chains: u64,
    pub snapshot_chains: u64,
    pub trees: u64,
    pub snapshot_trees: u64,
}

/// What an audit found. Every number is exact as of [`LeakReport::txn_id`] and no later: the
/// audit holds one read transaction from start to finish, so a commit that lands while it runs
/// is simply not part of the question it answered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LeakReport {
    /// The transaction the file was at when the audit began.
    pub txn_id: TxnId,
    /// `meta.page_count` - the universe of the question.
    pub page_count: u64,
    /// What the backing file actually holds. Larger than `page_count` means pages the meta
    /// page does not account for; see [`LeakReport::beyond_meta`].
    pub file_pages: u64,
    /// Pages something points at.
    pub reachable: u64,
    /// Pages the freelist holds, reusable or not.
    pub free_total: u64,
    /// The subset of those a writer could take right now.
    pub free_reusable: u64,
    /// **Pages nothing points at and nothing has recorded as free.** Space the file will never
    /// use and never give back.
    pub leaked: u64,
    /// The highest of them. This is the number that explains a `truncate_tail` that released
    /// nothing: every free page below it is pinned by it.
    pub highest_leaked: Option<Pgno>,
    /// The lowest few, for somebody who wants to go and look at them.
    ///
    /// **Not an input to anything that frees.** These page numbers were derived under a read
    /// transaction while writers were free to commit; a later commit may have put any of them
    /// to work. Acting on this list requires re-deriving it under the write lock with no
    /// readers alive, the way `Store::truncate_tail` does.
    pub leaked_sample: Vec<Pgno>,
    /// Pages in the file past what the meta page accounts for. Nonzero after a crash between
    /// the meta write and the truncation in `truncate_tail`; those pages are absorbed on the
    /// next open and never recovered.
    pub beyond_meta: u64,
    /// References to pages at or beyond the end of the file. **Always a damaged file.**
    pub dangling: u64,
    /// Pages both reachable and *reusable*-free - a page that has been handed out twice.
    ///
    /// Reachable-and-free on its own is ordinary: a page a snapshot pins is reachable from
    /// that snapshot's tree and sits in the freelist as a pending run. Restricting to reusable
    /// runs removes exactly that case, because a snapshot at transaction `S` holds the horizon
    /// at or below `S` while everything its tree reaches was freed after `S`. What is left is
    /// corruption, and nothing else in this tree looks for it.
    pub double_allocated: u64,
    pub double_allocated_sample: Vec<Pgno>,
    pub by_class: AuditTally,
}

impl LeakReport {
    /// Whether the file accounts for every page it contains.
    pub fn is_clean(&self) -> bool {
        self.leaked == 0
            && self.beyond_meta == 0
            && self.dangling == 0
            && self.double_allocated == 0
    }
}

/// How many pages a run covers, clamped into the file.
pub(crate) fn run_pages(run: &FreeRun, limit: u64) -> impl Iterator<Item = Pgno> {
    let first = run.first as u64;
    let end = (first + run.len as u64).min(limit);
    (first..end).map(|p| p as Pgno)
}
