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

//! Reading a written value as the fact its field's kind makes it.
//!
//! **The field's kind decides how a value is read, and this is the one place that decides it.**
//! There are two ways into this engine - `POST /table/{t}/import`, which sends
//! `field record value` a line at a time, and a SQL `INSERT`, which sends literals - and if
//! they disagreed about what `true` means on a boolean field, or about how many units `12.50`
//! is on a decimal, then one table would hold two conventions and nothing downstream could tell
//! which line wrote which. So both spellings land here, and the second is written in terms of
//! the first wherever it can be.

use crate::error::ApiError;
use crate::{Fact, FieldInfo};
use big_db::catalog::FieldKind;
use big_db::RecordId;
use big_plan::Literal;
use big_plan::PlanError;
use big_sql::SqlError;

/// What a written value can be wrong about, for a field of a given kind.
///
/// **No `String` in it.** The happy path of an import runs once per fact over bodies of
/// millions, so the error path is the only one that may allocate: this is the fact that
/// something was wrong, and [`ValueError::why`] is the sentence, built on the way out.
// **Not , because one variant carries the members it refused against.** Naming the legal
// values is most of what that error is worth - a closed list is short, and "which values, then"
// is the only question it raises - so the list is carried and the trait is not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValueError {
    /// An unsigned integer field, given something that is not a whole number.
    NeedsNumber,
    /// A signed integer field, given something that is not a number.
    NeedsSignedNumber,
    /// A boolean field, given something that is not `true` or `false`.
    NeedsBool,
    /// A time quantum field, given `key@` and something that is not a moment.
    NeedsSeconds,
    /// A keyed field, given something that is not a string.
    NeedsKey,
    /// An enum column, given a value it was not declared to hold.
    ///
    /// **The whole of what declaring the members buys**, and the reason it is a refusal rather
    /// than a stored value: a mutex takes any string, so without this the declaration would be
    /// a comment. Carries the members so the message can name them - a closed list is short by
    /// construction, and "which values, then" is the only question this error raises.
    NotAMember {
        /// The values the column was declared with, in the order written.
        members: Vec<String>,
    },
    /// A float field, given something that is not a number.
    NeedsFloat,
    /// A float field, given a number it cannot hold: a NaN, or a magnitude past the range of a
    /// single-precision column. Refused rather than stored as an infinity, for the reason a
    /// too-wide integer is refused rather than truncated.
    FloatOutOfRange,
    /// A date field, given a string that is not a date it can hold.
    NeedsDate {
        /// How a date for this field is written, e.g. `YYYY-MM-DD`.
        want: &'static str,
    },
    /// A decimal field, given more digits after the point than it stores.
    ///
    /// Carries both numbers because this is the planner's own refusal reached from the write
    /// side - `WHERE price = 12.523` and `VALUES (12.523)` are the same mistake about the same
    /// field, and [`ValueError::into_error`] gives them the same error to prove it.
    TooPrecise {
        /// How many digits after the point the value was written with.
        written: u8,
        /// How many the field keeps.
        scale: u8,
    },
}

impl ValueError {
    /// The sentence, naming the field and what it was given.
    ///
    /// Built here rather than at each call site so that the same mistake reads the same whether
    /// it arrived on an import line or in a `VALUES` list.
    pub fn why(self, field: &str, value: &str) -> String {
        match self {
            Self::NeedsNumber => format!("`{field}` needs a number, got `{value}`"),
            Self::NeedsSignedNumber => {
                format!("`{field}` needs a signed number, got `{value}`")
            }
            Self::NeedsBool => format!("`{field}` needs true or false, got `{value}`"),
            Self::NeedsSeconds => {
                format!("`{field}` takes `key@seconds`, and `{value}` is not a number")
            }
            Self::NeedsKey => format!("`{field}` needs a key, got `{value}`"),
            Self::NotAMember { members } => {
                let list = members.iter().map(|m| format!("'{m}'")).collect::<Vec<_>>().join(", ");
                format!("`{field}` was declared to hold one of {list}, got `{value}`")
            }
            Self::NeedsFloat => format!("`{field}` needs a number, got `{value}`"),
            Self::FloatOutOfRange => {
                format!("`{field}` cannot hold `{value}`")
            }
            Self::NeedsDate { want } => {
                format!("`{field}` takes a date written `{want}`, and `{value}` is not one")
            }
            // The planner's sentence, because it is the planner's refusal - see `into_error`.
            Self::TooPrecise { written, scale } => {
                PlanError::TooPrecise { field: field.to_string(), written, scale }.to_string()
            }
        }
    }

    /// The error a caller reports, which for one of these is not this crate's to invent.
    ///
    /// **A value with more digits than its field keeps is the refusal `WHERE price = 12.523`
    /// already gives**, and it arrives here only from a SQL `INSERT` - an import line carries
    /// no scale, so it can never be too precise. Handing back the planner's own error is what
    /// makes one mistake read one way: the same code, `too_precise`, and the same sentence,
    /// whichever half of the statement wrote the number.
    ///
    /// Everything else is a value the field cannot hold, which is neither a planning failure
    /// nor a storage one - see [`ApiError::Value`].
    pub fn into_error(self, field: &str, value: &str) -> ApiError {
        match self {
            Self::TooPrecise { written, scale } => {
                ApiError::Sql(SqlError::Plan(PlanError::TooPrecise {
                    field: field.to_string(),
                    written,
                    scale,
                }))
            }
            other => ApiError::Value(other.why(field, value)),
        }
    }
}

/// One fact, from a value written as text - the `field record value` line format.
///
/// `field` is borrowed and so is a keyed value, which is what keeps an import from allocating
/// twice per fact on the one route whose purpose is volume.
#[inline]
pub fn from_text<'a>(
    field: &'a str,
    info: &FieldInfo,
    record: RecordId,
    value: &'a str,
) -> Result<Fact<'a>, ValueError> {
    Ok(match info.kind {
        FieldKind::SignedInt => match value.parse::<i64>() {
            Ok(v) => Fact::Signed { field, record, value: v },
            Err(_) => return Err(ValueError::NeedsSignedNumber),
        },
        // **A decimal is read as the units it stores**, because that is what a line of an
        // import has always meant and changing it would rewrite what existing clients send.
        // A SQL `INSERT` writes `12.50` and means the same 1250 - see [`from_literal`], which
        // is where the two spellings meet.
        FieldKind::Int | FieldKind::Decimal => match value.parse::<u64>() {
            Ok(v) => Fact::Int { field, record, value: v },
            Err(_) => return Err(ValueError::NeedsNumber),
        },
        // Written the way a float is written. An import line has no field to consult, so this
        // is the one spelling, and it is the one `f64::from_str` reads.
        FieldKind::Float32 | FieldKind::Float64 => match value.parse::<f64>() {
            Ok(v) => float_fact(field, info, record, v)?,
            Err(_) => return Err(ValueError::NeedsFloat),
        },
        // **A date is written as a date, not as a count.** An integer field takes the units it
        // stores because that is what an import line has always sent, but a date has never been
        // sendable at all, so there is no older spelling to keep faith with - and `2024-01-15`
        // is what anyone writing one will send.
        FieldKind::Date | FieldKind::DateTime => match date_unit(info.kind).to_count(value) {
            Some(n) => Fact::Signed { field, record, value: n },
            None => return Err(ValueError::NeedsDate { want: date_unit(info.kind).format() }),
        },
        FieldKind::Bool => match value {
            "true" => Fact::Bool { field, record, value: true },
            "false" => Fact::Bool { field, record, value: false },
            _ => return Err(ValueError::NeedsBool),
        },
        // **A time quantum field takes `key@seconds`.** Without a moment the field can be
        // filled and still have no views by day for a window to read. A key with no `@` is
        // still a key: the views are an addition, not a requirement.
        FieldKind::TimeQuantum => match value.rsplit_once('@') {
            None => Fact::Key { field, record, value },
            Some((key, at)) => match at.parse::<i64>() {
                Ok(unix_seconds) => Fact::Time { field, record, value: key, unix_seconds },
                Err(_) => return Err(ValueError::NeedsSeconds),
            },
        },
        _ => {
            // The other funnel a written value comes through - the import route's `field record
            // value` lines - so the declaration is enforced on both or on neither.
            member_check(info, value)?;
            Fact::Key { field, record, value }
        }
    })
}

/// Refuses a value an enum column was not declared to hold.
///
/// A field with no members is not an enum, so this is the whole of the cost for every other
/// column: one `is_empty` on a `Vec` that is empty. The scan is linear because the list is
/// short by construction - a closed set somebody typed into a column definition - and building
/// an index per write would cost more than it saved.
fn member_check(info: &FieldInfo, value: &str) -> Result<(), ValueError> {
    if info.members.is_empty() || info.members.iter().any(|m| m == value) {
        return Ok(());
    }
    Err(ValueError::NotAMember { members: info.members.clone() })
}

/// One fact, from a value written as a SQL literal.
///
/// The same table as [`from_text`], against the types a parser already told apart. The one
/// place the two differ is a decimal: a literal carries its own scale, so `12.50` is converted
/// into the field's units here and means exactly what `WHERE price >= 12.50` means. An import
/// line has no scale to carry and sends units, which is what it has always sent.
#[inline]
pub fn from_literal<'a>(
    field: &'a str,
    info: &FieldInfo,
    record: RecordId,
    value: &'a Literal,
) -> Result<Fact<'a>, ValueError> {
    Ok(match (info.kind, value) {
        (FieldKind::SignedInt, Literal::Sint(v)) => Fact::Signed { field, record, value: *v },
        (FieldKind::SignedInt, Literal::Int(v)) => match i64::try_from(*v) {
            Ok(v) => Fact::Signed { field, record, value: v },
            Err(_) => return Err(ValueError::NeedsSignedNumber),
        },
        (FieldKind::SignedInt, _) => return Err(ValueError::NeedsSignedNumber),

        (FieldKind::Int, Literal::Int(v)) => Fact::Int { field, record, value: *v },
        (FieldKind::Int, _) => return Err(ValueError::NeedsNumber),

        // The planner's own conversion, so that a value written into a decimal field and a
        // value compared against one are scaled by the same code.
        (FieldKind::Decimal, Literal::Int(_) | Literal::Dec { .. }) => {
            match big_plan::to_units(field, value, info.scale.max(0) as u8) {
                Ok(value) => Fact::Int { field, record, value },
                Err(PlanError::TooPrecise { written, scale, .. }) => {
                    return Err(ValueError::TooPrecise { written, scale })
                }
                // The only other way that conversion fails is a number too large to scale,
                // which is the same thing an over-wide value into any integer field is.
                Err(_) => return Err(ValueError::NeedsNumber),
            }
        }
        (FieldKind::Decimal, _) => return Err(ValueError::NeedsNumber),

        // Every numeric literal, including the negative fractional one only a float takes.
        // `Literal::as_f64` is the same conversion the planner makes for a bound, so a value
        // written into a float field and a value compared against one meet as the same number.
        (
            FieldKind::Float32 | FieldKind::Float64,
            Literal::Int(_) | Literal::Sint(_) | Literal::Dec { .. } | Literal::Sdec { .. },
        ) => {
            let v = value.as_f64().expect("guarded by the match arm");
            float_fact(field, info, record, v)?
        }
        (FieldKind::Float32 | FieldKind::Float64, _) => return Err(ValueError::NeedsFloat),

        (FieldKind::Date | FieldKind::DateTime, Literal::Str(s)) => {
            match date_unit(info.kind).to_count(s) {
                Some(n) => Fact::Signed { field, record, value: n },
                None => return Err(ValueError::NeedsDate { want: date_unit(info.kind).format() }),
            }
        }
        (FieldKind::Date | FieldKind::DateTime, _) => {
            return Err(ValueError::NeedsDate { want: date_unit(info.kind).format() })
        }

        (FieldKind::Bool, Literal::Bool(v)) => Fact::Bool { field, record, value: *v },
        (FieldKind::Bool, _) => return Err(ValueError::NeedsBool),

        // Written in terms of `from_text` rather than beside it: `key@seconds` is one spelling
        // and it is read by one function, so the two cannot come to disagree about where the
        // `@` is.
        (FieldKind::TimeQuantum, Literal::Str(s)) => from_text(field, info, record, s)?,
        (FieldKind::TimeQuantum, _) => return Err(ValueError::NeedsKey),

        (_, Literal::Str(s)) => {
            // Checked here rather than at the storage layer, which sees a mutex and has no list
            // to check against: the declaration lives in the catalog beside the field, and this
            // is the one funnel every written value passes through.
            member_check(info, s)?;
            Fact::Key { field, record, value: s.as_str() }
        }
        (_, _) => return Err(ValueError::NeedsKey),
    })
}

/// One float fact, refused rather than rounded to an infinity when the field cannot hold it.
///
/// Its own function because both spellings reach it - an import line and a SQL literal - and a
/// value the field cannot hold has to be the same refusal either way.
#[inline]
fn float_fact<'a>(
    field: &'a str,
    info: &FieldInfo,
    record: RecordId,
    value: f64,
) -> Result<Fact<'a>, ValueError> {
    let declared = if info.bit_depth == 0 { 64 } else { info.bit_depth };
    match big_db::float::encode(value, declared) {
        Some(_) => Ok(Fact::Float { field, record, bits: value.to_bits() }),
        None => Err(ValueError::FloatOutOfRange),
    }
}

/// Which count a temporal field keeps. The kinds are two and the units are two, so this is a
/// mapping rather than a decision.
fn date_unit(kind: FieldKind) -> big_plan::TimeUnit {
    match kind {
        FieldKind::Date => big_plan::TimeUnit::Days,
        _ => big_plan::TimeUnit::Seconds,
    }
}

/// How a literal reads back in a message, which is the text a client wrote.
pub fn written(value: &Literal) -> String {
    match value {
        Literal::Int(n) => n.to_string(),
        Literal::Sint(n) => n.to_string(),
        // The same placing of the point a decimal cell is rendered with, so a value reads back
        // in an error message exactly as it would in an answer.
        Literal::Dec { units, scale } => crate::result::fixed(i128::from(*units), *scale),
        Literal::Sdec { units, scale } => crate::result::fixed(i128::from(*units), *scale),
        Literal::Str(s) => s.clone(),
        Literal::Bool(b) => b.to_string(),
    }
}
