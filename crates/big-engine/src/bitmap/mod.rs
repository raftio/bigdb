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

//! The bitmap engine: fragments of roaring containers, and the field conventions over them.
//!
//! A **fragment** is one b-tree; a **row** is a contiguous span of its container keys. Because
//! a row occupies a known contiguous range, reading one is a range scan rather than a lookup
//! per chunk, and unioning two is a merge of two scans.
//!
//! A fragment is addressed by [`FragmentKey`] - table, field, view, shard - and each of those
//! four exists because something partitions on it independently. In particular **shard** is what
//! bounds how much of the tree a write touches and what lets a read fan out across threads. The
//! coordinate arithmetic itself is in [`crate::base::coords`], because shard and record addressing is
//! not the bitmap engine's alone: the columnar engine and the cluster both partition on it.
//!
//! [`field`] is the other half of this engine and the reason one b-tree serves five field kinds:
//! a field type is a *mapping* from a value to the rows it turns on, and the fragment underneath
//! does not know which convention produced the bits it holds.

use crate::base::engine::{Engine, Fact, Half, Sink};
use crate::base::field_kind::FieldKind;

pub mod field;
pub mod read;
pub mod rowset;
pub mod write;

pub use crate::base::coords::*;
pub use big_container::Container;
pub use big_page::{ContainerKey, FragmentKey};
pub use read::FragmentRead;
pub use rowset::RowSet;
pub use write::FragmentWrite;

// The field conventions, at the engine that stores what they produce. `field::` stays a path in
// its own right for a caller that wants to be explicit about which half it is reaching for.
pub use field::{
    day_view, BoolField, Bsi, DateTime, FieldError, Granularity, MutexField, RangeOp, SetField,
    DAY_VIEW_LEN, DEFAULT_GRANULARITY, EXISTS_ROW,
};

/// The descriptor. See [`crate::base::engine`] for what a descriptor is and is not.
pub struct BitmapEngine;

impl Engine for BitmapEngine {
    fn code(&self) -> u8 {
        0
    }

    fn name(&self) -> &'static str {
        "bitmap"
    }

    fn has_bitmap(&self) -> bool {
        true
    }

    fn has_columns(&self) -> bool {
        false
    }

    /// Every fact becomes bits, and which bits is what [`field`] decides.
    ///
    /// The zone map is observed even here, where there is no scan to skip a shard: an index uses
    /// it to rule a fragment out without reading a page, which is the same saving from the other
    /// direction.
    fn place(&self, sink: &mut dyn Sink, fact: Fact<'_>) {
        sink.observe(Half::Bitmap, fact.observed());
        match fact {
            // Left unexpanded. The bit depth can still grow later in the transaction, and
            // expanding now would write a record at a depth that turns out to be too narrow.
            Fact::Value { value, .. } => sink.planes(value),
            // Two rows, and the second is a *clear*: a boolean replaces, so the row it is
            // leaving has to be emptied or the record would read as both.
            Fact::Bool(b) => {
                sink.bit(BoolField::row_of(b), true);
                sink.bit(BoolField::row_of(!b), false);
            }
            // A mutex allows one value per record, and finding the one being replaced is a read.
            // A set only ever turns bits on, so it can wait with the rest of the batch.
            Fact::Row { row, kind, views } => {
                if kind == FieldKind::Mutex {
                    sink.mutex(row);
                } else {
                    sink.bit(row, true);
                }
                // One extra view per declared granularity, which is what lets a range of days
                // read the days it asks about instead of every record ever written.
                for view in views {
                    sink.bit_in(*view, row, true);
                }
            }
        }
    }
}
