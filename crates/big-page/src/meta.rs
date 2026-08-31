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

//! Meta page: an alternating 0/1 pair. The one with the higher txn_id and a valid checksum wins.

use crate::error::PageError;
use crate::layout::*;
use crate::page::*;

/// Little-endian on disk, so the first four bytes of a file read `0BIG`.
pub const MAGIC: u32 = 0x4749_4230;
pub const VERSION: u32 = 1;
/// Two alternating meta pages; a commit writes to `txn_id % 2`.
pub const META_PAGES: u64 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MetaPage {
    pub txn_id: TxnId,
    pub page_count: u64,
    pub root_records: Option<Pgno>,
    pub freelist: Option<Pgno>,
    pub snapshots: Option<Pgno>,
    /// Opaque to the storage layer: schema and per-fragment metadata live here.
    pub catalog: Option<Pgno>,
    pub flags: u32,
}

/// `pgno == 0` is a meta page, so it doubles as the "absent" sentinel inside the meta page.
fn dec_pgno(v: u32) -> Option<Pgno> {
    (v != 0).then_some(v)
}

impl MetaPage {
    pub fn decode(page: &Page) -> Result<Self, PageError> {
        page.verify_checksum()?;
        let b = &page.0;
        let rd32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let rd64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());

        let magic = rd32(0);
        if magic != MAGIC {
            return Err(PageError::BadMagic(magic));
        }
        let version = rd32(4);
        if version != VERSION {
            return Err(PageError::UnsupportedVersion(version));
        }
        let page_size = rd32(8);
        if page_size as usize != PAGE_SIZE {
            return Err(PageError::PageSizeMismatch {
                expected: PAGE_SIZE as u32,
                found: page_size,
            });
        }
        Ok(Self {
            flags: rd32(12),
            txn_id: rd64(16),
            page_count: rd64(24),
            root_records: dec_pgno(rd32(32)),
            freelist: dec_pgno(rd32(36)),
            snapshots: dec_pgno(rd32(40)),
            catalog: dec_pgno(rd32(44)),
        })
    }

    pub fn encode(&self) -> Page {
        let mut page = Page::zeroed();
        page.set_header(self.slot() as Pgno, PageType::Meta, 0);
        let b = &mut page.0;
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&VERSION.to_le_bytes());
        b[8..12].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        b[12..16].copy_from_slice(&self.flags.to_le_bytes());
        b[16..24].copy_from_slice(&self.txn_id.to_le_bytes());
        b[24..32].copy_from_slice(&self.page_count.to_le_bytes());
        b[32..36].copy_from_slice(&self.root_records.unwrap_or(0).to_le_bytes());
        b[36..40].copy_from_slice(&self.freelist.unwrap_or(0).to_le_bytes());
        b[40..44].copy_from_slice(&self.snapshots.unwrap_or(0).to_le_bytes());
        b[44..48].copy_from_slice(&self.catalog.unwrap_or(0).to_le_bytes());
        page.seal();
        page
    }

    /// The magic overwrites where the shared header keeps `pgno`, so the slot comes from txn_id.
    pub fn slot(&self) -> u64 {
        self.txn_id % META_PAGES
    }
}

/// Picks the winning meta of the two slots. This is the whole of crash recovery:
/// pure copy-on-write means there is never anything to replay.
pub fn pick_meta(
    a: Result<MetaPage, PageError>,
    b: Result<MetaPage, PageError>,
) -> Result<MetaPage, PageError> {
    match (a, b) {
        (Ok(x), Ok(y)) => Ok(if x.txn_id >= y.txn_id { x } else { y }),
        (Ok(x), Err(_)) => Ok(x),
        (Err(_), Ok(y)) => Ok(y),
        (Err(e), Err(_)) => Err(e),
    }
}
