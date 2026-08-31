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

//! The unit a leaf stores: one container, either inline or as a pointer to a dense page.

use big_page::{ContainerKey, ContainerType, LeafBuilder, LeafCell, Pgno};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LeafItem {
    pub key: ContainerKey,
    pub ty: ContainerType,
    pub elem_n: u16,
    pub cardinality: u32,
    pub bitmap_checksum: u32,
    pub bitmap_pgno: Pgno,
    pub payload: Vec<u8>,
}

impl LeafItem {
    pub fn from_cell(c: &LeafCell<'_>) -> Self {
        Self {
            key: c.key,
            ty: c.ty,
            elem_n: c.elem_n as u16,
            cardinality: c.cardinality,
            bitmap_checksum: c.bitmap_checksum,
            bitmap_pgno: c.bitmap_pgno,
            payload: c.payload.to_vec(),
        }
    }

    pub fn dense(key: ContainerKey, pgno: Pgno, cardinality: u32, checksum: u32) -> Self {
        Self {
            key,
            ty: ContainerType::BitmapPtr,
            elem_n: 0,
            cardinality,
            bitmap_checksum: checksum,
            bitmap_pgno: pgno,
            payload: Vec::new(),
        }
    }

    /// A dense container plus the bits changed since its page was last written whole.
    pub fn delta(
        key: ContainerKey,
        pgno: Pgno,
        checksum: u32,
        cardinality: u32,
        entries: &[big_container::DeltaEntry],
    ) -> Self {
        Self {
            key,
            ty: ContainerType::BitmapDelta,
            elem_n: entries.len() as u16,
            cardinality,
            bitmap_checksum: checksum,
            bitmap_pgno: pgno,
            payload: big_container::delta::encode(entries),
        }
    }

    /// Whether this item owns a page of its own.
    ///
    /// Both dense forms do, and every caller that frees, copies or walks pages has to treat them
    /// alike - a delta cell's base page is just as reachable-and-owned as a plain one's.
    pub fn is_dense(&self) -> bool {
        self.ty.owns_page()
    }

    /// The page this item's container is based on, if it has one.
    pub fn base_pgno(&self) -> Option<Pgno> {
        self.is_dense().then_some(self.bitmap_pgno)
    }

    pub(crate) fn push_into(&self, b: &mut LeafBuilder) -> Option<()> {
        b.push(
            self.key,
            self.ty,
            self.elem_n,
            self.cardinality,
            self.bitmap_checksum,
            self.bitmap_pgno,
            &self.payload,
        )
    }
}
