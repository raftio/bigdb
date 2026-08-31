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

//! What a planner needs from a database, and nothing else.

/// How a field behaves in a query, which is coarser than how it is stored.
///
/// A planner does not care that an integer is a bit-sliced index or that a decimal carries a
/// scale; it cares whether `> 5` is meaningful and whether `= "GB"` is. Keeping this coarse is
/// what lets the storage side change field kinds without the parser noticing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldClass {
    /// Comparable with an ordering: `>`, `>=`, `<`, `<=`, `==`, `!=`.
    ///
    /// `scale` is how many digits of the stored integer sit after the decimal point. Zero for
    /// a plain integer. The planner uses it to turn a written value into the units actually
    /// stored, which is the whole reason it has to travel this far up.
    Integer { scale: u8 },
    /// The same comparisons, over a value that may be negative.
    ///
    /// Kept apart from `Integer` rather than given it a `signed` flag, because the planner has
    /// to make one decision differently: `-1` is a legal bound here and a mistake there, and a
    /// flag would let that decision be forgotten at a `match` arm that compiles either way.
    Signed,
    /// Compared against a string key for equality only.
    ///
    /// Carries which kind of keyed field it is, because two decisions depend on it and both
    /// were previously made wrong by not being able to tell. A window reads views only a time
    /// quantum field writes, and a record's own value can be read back only where a mutex keeps
    /// the shadow that says which row it is in.
    Keyed(Keyed),
    /// True or false.
    Boolean,
}

/// Which kind of keyed field, for the two questions that turn on it.
///
/// **This distinction used to be missing, and its absence was a wrong answer rather than a
/// missing feature.** `Row(f="k", from=…, to=…)` reads the day views a time quantum field
/// writes; against a plain set field there are none, so it answered *empty* - a set of no
/// records, indistinguishable from a window that genuinely matched nothing. The planner could
/// not refuse it because the planner could not see the difference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keyed {
    /// Any number of values per record.
    Set,
    /// At most one, with a shadow index from a record to the row it sits in.
    Mutex,
    /// A set, with views by day, so a window over it reads those rather than everything the
    /// field ever recorded.
    Time,
}

impl Keyed {
    /// Whether a window over this field has views to read.
    pub fn has_time_views(self) -> bool {
        matches!(self, Self::Time)
    }
}

/// The database, as far as planning is concerned.
///
/// Implemented on the storage side, so this crate never links it.
pub trait Schema {
    fn has_table(&self, table: &str) -> bool;

    /// `None` when the field does not exist on that table.
    fn field_class(&self, table: &str, field: &str) -> Option<FieldClass>;

    /// Whether this table keeps its values in column segments as well as, or instead of, an
    /// index.
    ///
    /// The planner needs exactly one bit of the storage engine and this is it: a keyed column
    /// can be projected only where the values are actually stored, because a bitmap records
    /// *which records hold a key* and never *which key a record holds*. Everything else about
    /// the engine choice is a matter of cost, which is not the planner's to decide.
    ///
    /// Defaulted to `false` so that a `Schema` written before this existed keeps refusing
    /// exactly what it refused before, rather than promising a read it cannot serve.
    fn stores_values(&self, _table: &str) -> bool {
        false
    }
}
