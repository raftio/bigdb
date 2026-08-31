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

//! Root records: `FragmentKey -> root pgno`. A flat list, rewritten on every commit.

use big_page::{chain_capacity, FragmentKey, Pgno, RootRecord, ROOT_RECORD_BYTES};
use std::collections::BTreeMap;

pub const ROOTS_PER_PAGE: usize = chain_capacity(ROOT_RECORD_BYTES);

/// `BTreeMap`, not `HashMap`: key order is exactly the prefix-scan order callers rely on.
#[derive(Clone, Default, Debug)]
pub struct RootRecords {
    map: BTreeMap<FragmentKey, Pgno>,
}

impl RootRecords {
    pub fn from_entries(entries: &[Vec<u8>]) -> Self {
        let mut map = BTreeMap::new();
        for e in entries {
            let r = RootRecord::decode(e);
            map.insert(r.key, r.root);
        }
        Self { map }
    }

    pub fn get(&self, key: &FragmentKey) -> Option<Pgno> {
        self.map.get(key).copied()
    }

    pub fn set(&mut self, key: FragmentKey, root: Pgno) {
        self.map.insert(key, root);
    }

    pub fn remove(&mut self, key: &FragmentKey) -> Option<Pgno> {
        self.map.remove(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&FragmentKey, &Pgno)> {
        self.map.iter()
    }

    /// Every fragment in a key range: the basis for scanning one field across every shard.
    pub fn range(
        &self,
        lo: FragmentKey,
        hi: FragmentKey,
    ) -> impl Iterator<Item = (&FragmentKey, &Pgno)> {
        self.map.range(lo..hi)
    }

    pub fn encode(&self) -> Vec<Vec<u8>> {
        self.map.iter().map(|(k, v)| RootRecord { key: *k, root: *v }.encode().to_vec()).collect()
    }
}
