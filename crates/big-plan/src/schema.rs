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
    /// The same comparisons, over a value that has a fractional part the field rounds to.
    ///
    /// `bits` is the field's precision - 32 or 64 - and the planner needs it for the one
    /// decision a float makes differently from every other number: a threshold the field cannot
    /// hold exactly has to be rounded in the direction that does not drop a record, and `= 3.14`
    /// on a single-precision column answers nothing at all. See `big_db::float`.
    ///
    /// Kept apart from `Integer` rather than given it a flag for the reason [`Self::Signed`] is:
    /// `sum` over one of these is a scan rather than a walk over bit planes, and a flag would
    /// let that be forgotten at a `match` arm that compiles either way.
    Float { bits: u8 },
    /// Ordered like a number, written and read back as a date.
    ///
    /// The value stored is a count from the Unix epoch; `unit` says of what. Comparisons are
    /// against a written date rather than a number, which is the whole reason this is not
    /// [`Self::Signed`] - `'2024-01-01'` is a legal bound here and a mistake there.
    Temporal { unit: TimeUnit },
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

/// What a temporal field counts from the Unix epoch.
///
/// The two are one type rather than two field classes because every decision the planner makes
/// about them is the same one; only the conversion from a written date differs, and that is
/// arithmetic rather than a rule.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TimeUnit {
    /// A `DATE`: whole days, so a written time of day is a value it cannot hold.
    Days,
    /// A `DATETIME`: seconds.
    Seconds,
}

impl TimeUnit {
    /// A written date as the number this field stores, or `None` when it is not a date at all.
    ///
    /// **The one conversion**, so that a bound in a `WHERE` and a value in an `INSERT` cannot
    /// come to disagree about what `'2024-01-15'` is - which is the mistake the decimal path
    /// already made once and fixed by having exactly one `to_units`.
    pub fn to_count(self, written: &str) -> Option<i64> {
        match self {
            // A `DATE` takes only a date. Accepting `'2024-01-15 10:30:00'` and dropping the
            // time would silently answer about midnight, and a value that does not read back as
            // what was written is the bug this tree has already had once.
            Self::Days => big_civil::parse_date(written),
            Self::Seconds => big_civil::parse_datetime(written),
        }
    }

    /// How a date for this field is written, for the refusal that has to say so.
    pub fn format(self) -> &'static str {
        match self {
            Self::Days => "YYYY-MM-DD",
            Self::Seconds => "YYYY-MM-DD HH:MM:SS",
        }
    }

    /// The number this field stores, written back as the date it stands for.
    pub fn to_written(self, count: i64) -> String {
        match self {
            Self::Days => big_civil::format_date(count),
            Self::Seconds => big_civil::format_datetime(count),
        }
    }
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

    /// Every column the table declares, in the order it declared them.
    ///
    /// What `SELECT *` expands to, which is why it is here rather than left to the caller: the
    /// statement is written before anything knows the table, so the list has to be filled in
    /// where a schema is in reach.
    ///
    /// **Not defaulted.** An empty list is a real answer - a table with no fields - so a
    /// default would be a `Schema` silently answering `SELECT *` with no columns at all, which
    /// is a wrong answer rather than a missing one.
    fn fields(&self, table: &str) -> Vec<String>;

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

/// The columns `SELECT *` expands to on this table, in declaration order.
///
/// **One function because two callers have to agree.** The plan reads these columns and the
/// shape names them, and a plan reading three columns under a header of four is an answer that
/// is wrong rather than absent - so neither side works the list out for itself.
///
/// Empty where there is nothing a projection could read: a table with no fields, or one whose
/// engine keeps no values, where a keyed or boolean column has no read back from a record at
/// all. Both cases fall back to record ids, which is what `SELECT *` answered before it
/// expanded to anything.
pub fn expanded_columns(schema: &impl Schema, table: &str) -> Vec<String> {
    let stores_values = schema.stores_values(table);
    schema
        .fields(table)
        .into_iter()
        .filter(|f| match schema.field_class(table, f) {
            Some(
                FieldClass::Integer { .. }
                | FieldClass::Signed
                | FieldClass::Float { .. }
                | FieldClass::Temporal { .. },
            ) => true,
            Some(FieldClass::Keyed(_) | FieldClass::Boolean) => stores_values,
            None => false,
        })
        .collect()
}
