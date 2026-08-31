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

//! Field types: the convention that decides which row a value goes into.
//!
//! Nothing here stores anything. A field type is a *mapping* - given a value and a record,
//! which bits in which rows of a fragment does it turn on - and the fragment underneath does
//! not know which convention produced the bits it holds. That separation is what lets five
//! field kinds share one b-tree.
//!
//! [`bsi`] is the one the engine is built around: a `u64` as one row per bit, so a range query
//! is a boolean circuit over `bit_depth` planes rather than a scan of values. [`set`] only ever
//! turns bits on and can be buffered freely; [`mutex`] cannot, because enforcing at-most-one
//! value means reading the shadow view first; [`quantum`] writes an extra view per granularity
//! so a range of days reads only the days it asks about.

pub mod bsi;
pub mod error;
pub mod mutex;
pub mod quantum;
pub mod set;

pub use bsi::{Bsi, RangeOp, EXISTS_ROW};
pub use error::{FieldError, Result};
pub use mutex::MutexField;
pub use quantum::{
    day_view, decompose, views, DateTime, Granularity, DAY_VIEW_LEN, DEFAULT_GRANULARITY,
};
pub use set::{BoolField, SetField};
