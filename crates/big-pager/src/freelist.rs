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

//! A flat, run-length encoded freelist. Deliberately not a b-tree: under copy-on-write a
//! b-tree freelist would allocate pages in order to record freed pages.

use big_page::{chain_capacity, Pgno, TxnId};

/// freed_at u64 + first u32 + len u32.
pub const FREE_ENTRY_BYTES: usize = 16;
pub const FREE_PER_PAGE: usize = chain_capacity(FREE_ENTRY_BYTES);

/// A contiguous run of pages freed by the same transaction.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct FreeRun {
    pub freed_at: TxnId,
    pub first: Pgno,
    pub len: u32,
}

impl FreeRun {
    fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; FREE_ENTRY_BYTES];
        b[0..8].copy_from_slice(&self.freed_at.to_le_bytes());
        b[8..12].copy_from_slice(&self.first.to_le_bytes());
        b[12..16].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Self {
        Self {
            freed_at: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            first: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            len: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct Freelist {
    runs: Vec<FreeRun>,
}

impl Freelist {
    pub fn from_entries(entries: &[Vec<u8>]) -> Self {
        let mut f = Self { runs: entries.iter().map(|e| FreeRun::decode(e)).collect() };
        f.compact();
        f
    }

    pub fn runs(&self) -> &[FreeRun] {
        &self.runs
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Every page currently in the freelist, reclaimable or not.
    pub fn total_pages(&self) -> u64 {
        self.runs.iter().map(|r| r.len as u64).sum()
    }

    /// A page is reclaimable once no live reader can still see the transaction that replaced it.
    pub fn reusable_pages(&self, horizon: TxnId) -> u64 {
        self.runs.iter().filter(|r| r.freed_at <= horizon).map(|r| r.len as u64).sum()
    }

    pub fn pending_pages(&self, horizon: TxnId) -> u64 {
        self.total_pages() - self.reusable_pages(horizon)
    }

    pub fn push(&mut self, pgno: Pgno, freed_at: TxnId) {
        self.runs.push(FreeRun { freed_at, first: pgno, len: 1 });
    }

    /// Merges adjacent runs sharing a `freed_at`. This is where the run-length encoding pays off.
    pub fn compact(&mut self) {
        if self.runs.len() < 2 {
            return;
        }
        self.runs.sort_unstable();
        let mut merged: Vec<FreeRun> = Vec::with_capacity(self.runs.len());
        for r in self.runs.drain(..) {
            match merged.last_mut() {
                Some(prev)
                    if prev.freed_at == r.freed_at
                        && prev.first as u64 + prev.len as u64 == r.first as u64 =>
                {
                    prev.len += r.len;
                }
                _ => merged.push(r),
            }
        }
        self.runs = merged;
    }

    /// Takes one reclaimable page. `None` means the file has to grow.
    pub fn alloc(&mut self, horizon: TxnId) -> Option<Pgno> {
        let idx = self.runs.iter().position(|r| r.freed_at <= horizon)?;
        let run = &mut self.runs[idx];
        let pgno = run.first;
        run.first += 1;
        run.len -= 1;
        if run.len == 0 {
            self.runs.remove(idx);
        }
        Some(pgno)
    }

    /// Drops reclaimable runs that sit flush against the end of the file, and reports how many
    /// pages that frees. Repeats, because removing one run can expose the one before it.
    pub fn trim_tail(&mut self, page_count: u64, horizon: TxnId) -> u64 {
        let mut end = page_count;
        loop {
            let found = self
                .runs
                .iter()
                .position(|r| r.freed_at <= horizon && r.first as u64 + r.len as u64 == end);
            match found {
                Some(i) => {
                    end -= self.runs[i].len as u64;
                    self.runs.remove(i);
                }
                None => return page_count - end,
            }
        }
    }

    pub fn entry_count(&self) -> usize {
        self.runs.len()
    }

    pub fn encode(&self) -> Vec<Vec<u8>> {
        self.runs.iter().map(|r| r.encode()).collect()
    }
}
