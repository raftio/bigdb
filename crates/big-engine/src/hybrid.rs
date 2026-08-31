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

//! The bitmap+columnar engine: both trees, over the same facts.
//!
//! This module holds no storage code, and that is not an omission. A table under this engine
//! writes exactly what [`crate::bitmap`] writes and exactly what [`crate::columnar`] writes,
//! into fragments that differ only in their view - so there is nothing here for a third
//! implementation to be. What the module *is* is the place the combination is named and
//! declared, which is what the write path branches on and what the planner reads when it decides
//! whether a question is better answered by an index or by a scan.
//!
//! The trade it makes: every fact is stored twice. An index answers *which records* without
//! reading values; a segment answers *what a record holds* without reconstructing it from bit
//! planes. Neither can do the other's job cheaply, so a table that is asked both kinds of
//! question pays a second copy rather than paying a scan or a reconstruction on every query.
//!
//! It is the default for a new table - see [`crate::TableEngine::default`] - because a caller
//! who does not choose wants the engine that answers the widest range of questions well.

use crate::base::engine::{Engine, Fact, Sink};

/// The descriptor. See [`crate::base::engine`] for what a descriptor is and is not.
pub struct HybridEngine;

impl Engine for HybridEngine {
    fn code(&self) -> u8 {
        1
    }

    fn name(&self) -> &'static str {
        "bitmap+columnar"
    }

    fn has_bitmap(&self) -> bool {
        true
    }

    fn has_columns(&self) -> bool {
        true
    }

    /// Both, and literally so.
    ///
    /// **This is the whole module in one method.** A hybrid table is not a third way of storing a
    /// fact, it is the other two asked in turn - and writing it as the two calls rather than as a
    /// copy of what they do is what keeps that true as they change.
    fn place(&self, sink: &mut dyn Sink, fact: Fact<'_>) {
        crate::bitmap::BitmapEngine.place(sink, fact);
        crate::columnar::ColumnarEngine.place(sink, fact);
    }
}
