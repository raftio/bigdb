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

//! `FragmentKey`: a fixed binary key, replacing byte-sortable strings.

use crate::page::Pgno;

/// 4+4+4+8 packed. In memory it is 24 bytes because u64 forces align 8.
pub const FRAGMENT_KEY_BYTES: usize = 20;

/// Field order decides the natural prefix scan: "one field across every shard".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct FragmentKey {
    pub table: u32,
    pub field: u32,
    pub view: u32,
    pub shard: u64,
}

impl FragmentKey {
    pub fn new(table: u32, field: u32, view: u32, shard: u64) -> Self {
        Self { table, field, view, shard }
    }

    /// Little-endian like everything else in the file; ordering comes from derived `Ord`,
    /// never from memcmp.
    pub fn encode(&self) -> [u8; FRAGMENT_KEY_BYTES] {
        let mut b = [0u8; FRAGMENT_KEY_BYTES];
        b[0..4].copy_from_slice(&self.table.to_le_bytes());
        b[4..8].copy_from_slice(&self.field.to_le_bytes());
        b[8..12].copy_from_slice(&self.view.to_le_bytes());
        b[12..20].copy_from_slice(&self.shard.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Self {
        Self {
            table: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            field: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            view: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            shard: u64::from_le_bytes(b[12..20].try_into().unwrap()),
        }
    }
}

/// One root-records row: which fragment has its root on which page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RootRecord {
    pub key: FragmentKey,
    pub root: Pgno,
}

pub const ROOT_RECORD_BYTES: usize = 24;

impl RootRecord {
    pub fn encode(&self) -> [u8; ROOT_RECORD_BYTES] {
        let mut b = [0u8; ROOT_RECORD_BYTES];
        b[..FRAGMENT_KEY_BYTES].copy_from_slice(&self.key.encode());
        b[FRAGMENT_KEY_BYTES..].copy_from_slice(&self.root.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Self {
        Self {
            key: FragmentKey::decode(b),
            root: u32::from_le_bytes(b[FRAGMENT_KEY_BYTES..ROOT_RECORD_BYTES].try_into().unwrap()),
        }
    }
}
