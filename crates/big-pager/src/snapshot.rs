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

//! Snapshot registry. Two meta pages carry no history, so snapshots must be stored explicitly.

use big_page::{chain_capacity, Pgno, TxnId};

pub const SNAPSHOT_ENTRY_BYTES: usize = 64;
pub const SNAPSHOTS_PER_PAGE: usize = chain_capacity(SNAPSHOT_ENTRY_BYTES);
pub const SNAPSHOT_NAME_LEN: usize = 32;

/// A snapshot with this flag never expires; used to pin state before a risky job.
pub const SNAP_PINNED: u32 = 1;

pub type SnapshotId = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub txn_id: TxnId,
    pub root_records: Pgno,
    pub flags: u32,
    /// Unix seconds. Ignored when `SNAP_PINNED` is set.
    pub expires_at: u64,
    pub name: [u8; SNAPSHOT_NAME_LEN],
}

impl Snapshot {
    pub fn is_pinned(&self) -> bool {
        self.flags & SNAP_PINNED != 0
    }

    pub fn is_expired(&self, now: u64) -> bool {
        !self.is_pinned() && self.expires_at <= now
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|b| *b == 0).unwrap_or(SNAPSHOT_NAME_LEN);
        core::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    pub fn make_name(s: &str) -> [u8; SNAPSHOT_NAME_LEN] {
        let mut n = [0u8; SNAPSHOT_NAME_LEN];
        let b = s.as_bytes();
        let len = b.len().min(SNAPSHOT_NAME_LEN);
        n[..len].copy_from_slice(&b[..len]);
        n
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; SNAPSHOT_ENTRY_BYTES];
        b[0..8].copy_from_slice(&self.id.to_le_bytes());
        b[8..16].copy_from_slice(&self.txn_id.to_le_bytes());
        b[16..20].copy_from_slice(&self.root_records.to_le_bytes());
        b[20..24].copy_from_slice(&self.flags.to_le_bytes());
        b[24..32].copy_from_slice(&self.expires_at.to_le_bytes());
        b[32..64].copy_from_slice(&self.name);
        b
    }

    fn decode(b: &[u8]) -> Self {
        Self {
            id: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            txn_id: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            root_records: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            flags: u32::from_le_bytes(b[20..24].try_into().unwrap()),
            expires_at: u64::from_le_bytes(b[24..32].try_into().unwrap()),
            name: b[32..64].try_into().unwrap(),
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct SnapshotRegistry {
    snaps: Vec<Snapshot>,
    next_id: SnapshotId,
}

impl SnapshotRegistry {
    pub fn from_entries(entries: &[Vec<u8>]) -> Self {
        let snaps: Vec<Snapshot> = entries.iter().map(|e| Snapshot::decode(e)).collect();
        let next_id = snaps.iter().map(|s| s.id).max().map_or(1, |m| m + 1);
        Self { snaps, next_id }
    }

    pub fn all(&self) -> &[Snapshot] {
        self.snaps.as_slice()
    }

    pub fn get(&self, id: SnapshotId) -> Option<Snapshot> {
        self.snaps.iter().find(|s| s.id == id).copied()
    }

    pub fn create(
        &mut self,
        txn_id: TxnId,
        root_records: Pgno,
        expires_at: u64,
        name: &str,
        pinned: bool,
    ) -> Snapshot {
        let snap = Snapshot {
            id: self.next_id,
            txn_id,
            root_records,
            flags: if pinned { SNAP_PINNED } else { 0 },
            expires_at,
            name: Snapshot::make_name(name),
        };
        self.next_id += 1;
        self.snaps.push(snap);
        snap
    }

    pub fn remove(&mut self, id: SnapshotId) -> Option<Snapshot> {
        let i = self.snaps.iter().position(|s| s.id == id)?;
        Some(self.snaps.remove(i))
    }

    /// Drops expired snapshots; their pages become reclaimable at the next commit.
    pub fn expire(&mut self, now: u64) -> Vec<Snapshot> {
        let (dead, alive): (Vec<_>, Vec<_>) = self.snaps.iter().partition(|s| s.is_expired(now));
        self.snaps = alive;
        dead
    }

    /// The `txn_id` floor the freelist is not allowed to reclaim past.
    pub fn oldest_txn_id(&self) -> Option<TxnId> {
        self.snaps.iter().map(|s| s.txn_id).min()
    }

    pub fn len(&self) -> usize {
        self.snaps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snaps.is_empty()
    }

    pub fn encode(&self) -> Vec<Vec<u8>> {
        self.snaps.iter().map(|s| s.encode()).collect()
    }
}
