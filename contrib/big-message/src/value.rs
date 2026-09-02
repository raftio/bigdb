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

//! One value of one message.
//!
//! # Why this is a kind of *literal* and not a kind of *field*
//!
//! There is no `Value::Set` and no `Value::Mutex`, because those are field kinds and field
//! kinds live in the schema, on the server. What a caller has is a number, a string or a
//! boolean, and what the wire carries is a SQL literal - so that is what this enum is: the
//! literals `big_plan::Literal` has, in the spellings `big_sql`'s lexer reads.
//!
//! The server decides whether the literal fits the field, in `big_embed::fact::from_literal`,
//! and its refusal arrives with the field named. That division is why a field kind added to the
//! engine later needs no variant here: `Value::Text` already reaches every keyed field there
//! is, and a new numeric one is already reachable by `Value::Int`.

/// One value, borrowed for as long as it takes to render it into a statement.
///
/// Borrowed rather than owned because a producer's whole job is volume, and a message that
/// allocates a `String` per field allocates once per fact on the one path whose purpose is
/// throughput. The borrow ends inside `Producer::send`, which renders immediately.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value<'a> {
    /// A whole number, for an `INT` or a `DECIMAL` field.
    ///
    /// **A decimal reached this way is in the units it stores** only if the caller wrote it
    /// that way; the point of [`Value::Decimal`] is that it does not have to be. See there.
    Int(u64),

    /// A whole number that may be negative, for a `SIGNED` field.
    Signed(i64),

    /// A number with a fractional part, written out.
    ///
    /// **The right variant for a `DECIMAL` field.** A SQL literal carries its own scale, so
    /// `12.50` against a scale-2 field is the 1250 units it stores - the conversion is
    /// `big_embed::fact::from_literal`'s, and it is exact, where going through an `f64` would
    /// not be. This is also the one place this crate differs usefully from the import route,
    /// which has no scale to carry and so has always taken raw units.
    ///
    /// Refused unless it is written the way the lexer reads a number: an optional `-`, digits,
    /// optionally a `.` and more digits. No exponent - `big_sql::lex` has none.
    Decimal(&'a str),

    /// A number with a fractional part, for a `FLOAT32` or `FLOAT64` field.
    ///
    /// **Not every `f64` can be written as a literal this dialect reads.** A number is lexed as
    /// `units / 10^scale` with `units` a `u64` and `scale` a `u8`, and there is no exponent
    /// form - so `1e300` has no spelling, and neither has `NaN` or an infinity. Those are
    /// refused here, where the message that caused it can still be named, rather than at the
    /// server, where one bad value refuses a batch of eight thousand.
    ///
    /// Ordinary magnitudes - prices, rates, durations, coordinates - are unaffected.
    Float(f64),

    /// A string, for every keyed field: `SET`, `MUTEX`, and a `TIMEQUANTUM` with no moment.
    ///
    /// Also how a `DATE` or a `DATETIME` is written, because a date is written as a date:
    /// `2024-01-15`. The server reads it with the same `to_count` a `WHERE` uses.
    Text(&'a str),

    Bool(bool),

    /// A key and the moment it happened, for a `TIMEQUANTUM` field.
    ///
    /// Rendered as the one spelling that route has always taken, `key@unix_seconds`, which the
    /// server splits on the **last** `@` - so a key containing one is still safe. Without a
    /// moment the field is filled but has no views by day for a window to read, which is what
    /// makes this worth its own variant rather than leaving callers to build the string.
    Keyed {
        key: &'a str,
        at: i64,
    },
}
