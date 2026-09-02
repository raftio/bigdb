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

//! Every way a query can be rejected, and where.

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlanError {
    // Parsing: the text is not a query.
    Unexpected {
        at: usize,
        found: String,
        want: &'static str,
    },
    UnterminatedString {
        at: usize,
    },
    NumberTooLarge {
        at: usize,
    },
    /// A decimal written with a leading `-`. Decimal fields are unsigned, so there is nowhere
    /// to put it; a signed integer field is the shape that holds negative numbers.
    NegativeDecimal {
        at: usize,
    },
    /// The query nested deeper than [`crate::parse::MAX_DEPTH`].
    ///
    /// Its own variant rather than an `Unexpected`, because it is the only parse failure that
    /// is about the shape of the input rather than about a character in it, and because an
    /// operator seeing `query_too_deep` in a log is looking at something quite different from
    /// a typo.
    TooDeep {
        /// Where the limit was reached.
        at: usize,
        /// The limit itself.
        limit: usize,
    },
    TrailingInput {
        at: usize,
    },

    // Planning: the query is well formed but does not describe anything real.
    UnknownTable(String),
    UnknownField {
        table: String,
        field: String,
    },
    UnknownCall(String),
    /// The call exists but was handed the wrong number of arguments.
    Arity {
        call: &'static str,
        want: &'static str,
        got: usize,
    },
    /// The argument is the wrong shape, e.g. a count where a bitmap was needed.
    BadArgument {
        call: &'static str,
        want: &'static str,
    },
    /// `> "GB"` on a keyed field, or `= "GB"` on an integer one.
    OperatorNotAllowed {
        field: String,
        op: String,
        class: &'static str,
    },
    /// More digits after the point than the field stores. Refused rather than rounded: a
    /// rounded comparison answers a different question from the one that was asked.
    TooPrecise {
        field: String,
        written: u8,
        scale: u8,
    },
    /// `toDate` or `date_trunc` written over a column it says nothing about.
    ///
    /// Its own variant rather than a [`Self::BadDate`], because the mistake is the other way
    /// round: there is nothing wrong with the value, and what does not fit is the call written
    /// over the column.
    BadRounding {
        /// The call, as it was written.
        call: String,
        /// What it needed instead.
        why: &'static str,
    },
    /// A date field compared against a string that is not a date it can hold.
    ///
    /// Carries what was written because every way of getting this wrong looks the same from the
    /// outside - a typo, the wrong separator, a time of day against a `DATE`, the 30th of
    /// February - and the written text is what tells them apart.
    BadDate {
        field: String,
        written: String,
        /// What the field would have accepted, e.g. `YYYY-MM-DD`.
        want: &'static str,
    },
}

impl core::fmt::Display for PlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unexpected { at, found, want } => {
                write!(f, "at byte {at}: expected {want}, found `{found}`")
            }
            Self::UnterminatedString { at } => write!(f, "at byte {at}: unterminated string"),
            Self::NumberTooLarge { at } => write!(f, "at byte {at}: number does not fit in u64"),
            Self::NegativeDecimal { at } => write!(
                f,
                "at byte {at}: decimal fields are unsigned, so a negative decimal cannot be \
                 compared against one"
            ),
            Self::TrailingInput { at } => write!(f, "at byte {at}: trailing input after the query"),
            Self::TooDeep { at, limit } => {
                write!(f, "at byte {at}: the query nests deeper than the limit of {limit}")
            }
            Self::UnknownTable(t) => write!(f, "no table `{t}`"),
            Self::UnknownField { table, field } => write!(f, "no field `{field}` on `{table}`"),
            Self::UnknownCall(c) => write!(f, "no such call `{c}`"),
            Self::Arity { call, want, got } => {
                write!(f, "`{call}` takes {want}, got {got}")
            }
            Self::BadArgument { call, want } => write!(f, "`{call}` needs {want}"),
            Self::OperatorNotAllowed { field, op, class } => {
                write!(f, "`{op}` is not allowed on `{field}`, which is {class}")
            }
            Self::TooPrecise { field, written, scale } => {
                write!(f, "`{field}` stores {scale} decimal places, but the value has {written}")
            }
            Self::BadRounding { call, why } => write!(f, "`{call}` needs {why}"),
            Self::BadDate { field, written, want } => {
                write!(f, "`{field}` takes a date written `{want}`, and `{written}` is not one")
            }
        }
    }
}

/// A stable, machine-readable name for the failure.
///
/// The four parsing variants share `parse_error`: they differ in where the text went wrong,
/// which the message already says, and not in what the client should do about it.
impl PlanError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unexpected { .. }
            | Self::UnterminatedString { .. }
            | Self::NumberTooLarge { .. }
            | Self::NegativeDecimal { .. }
            | Self::TrailingInput { .. } => "parse_error",
            Self::TooDeep { .. } => "query_too_deep",
            Self::UnknownTable(_) => "unknown_table",
            Self::UnknownField { .. } => "unknown_field",
            Self::UnknownCall(_) => "unknown_call",
            Self::Arity { .. } => "bad_arity",
            Self::BadArgument { .. } => "bad_argument",
            Self::OperatorNotAllowed { .. } => "operator_not_allowed",
            Self::TooPrecise { .. } => "too_precise",
            Self::BadDate { .. } => "bad_date",
            Self::BadRounding { .. } => "bad_rounding",
        }
    }
}

impl core::error::Error for PlanError {}

pub type Result<T> = core::result::Result<T, PlanError>;
