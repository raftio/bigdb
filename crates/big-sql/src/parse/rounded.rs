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

//! `WHERE date_trunc('month', ts) = '2024-01-01'`, read as the range it means.
//!
//! # Why a rounding in a `WHERE` can be answered when nothing else can
//!
//! [`Refused::ScalarFilter`] says the true thing about the general case: a condition chooses a
//! set out of bitmaps before a single value has been read, so there is nothing for an expression
//! to run on. A rounding escapes that not by being evaluated but by being *inverted*. Every
//! value whose month is January is every value in `[2024-01-01, 2024-02-01)`, and a range on a
//! bit-sliced column is the read the field already has - so the statement is answered by asking
//! a different question with the same answer, off the bit planes, at no new cost.
//!
//! The rounding functions are the ones this is true of: each is non-decreasing, so the values
//! that round into a bucket are contiguous. `lower`, `substring` and `abs` are not, which is why
//! they keep the refusal.
//!
//! # Why the parser, and why it needs no schema
//!
//! The bounds are computed on the **written date**, never on the stored count. `'2024-02-01'`
//! goes out as a string, and `big_plan::Ctx::row` turns it into days or into seconds according
//! to the column's own class - the same conversion it already does for `ts >= '2024-02-01'`
//! written by hand. So one rewrite is correct for a `DATE` and for a `DATETIME` alike, and this
//! module needs to know neither. It is the argument [`crate::lower::cond::windows`] makes for
//! fusing a time window without knowing whether the field has time views: the class is the
//! planner's to check, and it still checks it.

use super::Parser;
use crate::ast::{Cond, Name, Rounding};
use crate::error::{Refused, Result};
use crate::lex::Tok;
use big_civil::Unit;
use big_plan::Literal;

/// A call this module can turn into a range, and the boundary it rounds to.
enum Round {
    /// `date_trunc(<unit>, <column>)`, whose unit is read from the statement.
    Trunc,
    /// `toDate(<column>)`, which is `date_trunc('day', ...)` under an older name. Answered for a
    /// `DATETIME`, where it is a real rounding, *and* for a `DATE`, where it is the identity -
    /// the range `[d, d + 1 day)` is `= d` on a column counting whole days, so one rewrite
    /// serves both and neither needs the class.
    Day,
    /// `toYear(<column>)`, compared against a year *number* rather than a date.
    ///
    /// The one extractor that is non-decreasing. `toMonth` and `toDayOfMonth` cycle - every
    /// January of every year answers `1` - so the values behind an answer are not contiguous and
    /// there is no range to rewrite them into. They keep the refusal.
    Year,
    /// `round(<column>[, <digits>])`, `floor(<column>)`, `ceil(<column>)`.
    ///
    /// **Carried rather than computed here**, which is where a number parts company with a date.
    /// See [`Cond::Rounded`] for why: a date's two bounds are written dates and mean the same
    /// thing to either temporal class, while a number's depend on the field's scale.
    Number(Rounding),
}

impl Round {
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            n if n.eq_ignore_ascii_case("date_trunc") || n.eq_ignore_ascii_case("dateTrunc") => {
                Self::Trunc
            }
            n if n.eq_ignore_ascii_case("toDate") => Self::Day,
            n if n.eq_ignore_ascii_case("toYear") => Self::Year,
            // Digits are filled in below, where the second argument can be read.
            n if n.eq_ignore_ascii_case("round") => Self::Number(Rounding::Round { digits: 0 }),
            n if n.eq_ignore_ascii_case("floor") => Self::Number(Rounding::Floor),
            n if n.eq_ignore_ascii_case("ceil") || n.eq_ignore_ascii_case("ceiling") => {
                Self::Number(Rounding::Ceil)
            }
            _ => return None,
        })
    }
}

impl Parser<'_> {
    /// A rounding compared against a value, as the range it selects.
    ///
    /// `None` when the call is not one of these, which leaves the caller to refuse it. The `(`
    /// is still ahead; `name` has already been read as though it were a column, which is what
    /// every call in a `WHERE` looks like until its name is checked.
    pub(super) fn rounded(&mut self, name: &Name, at: usize) -> Result<Option<Cond>> {
        let Some(round) = Round::of(&name.column) else { return Ok(None) };
        self.i += 1;

        // A number's rounding is read and handed on; only its digits are settled here, because
        // only here is the second argument still in the text.
        if let Round::Number(kind) = round {
            let column = self.name("a column name")?;
            let round = match (kind, self.eat(&Tok::Comma)) {
                (Rounding::Round { .. }, true) => match self.literal("a number of digits")? {
                    Literal::Int(d) if d <= u64::from(u8::MAX) => {
                        Rounding::Round { digits: d as u8 }
                    }
                    _ => return Err(self.syntax("a number of digits")),
                },
                // `floor(x, 2)` is not a rounding this dialect has, and reading the argument as
                // digits would answer a question nobody asked.
                (_, true) => return Err(self.syntax(") to close the rounding")),
                (kind, false) => kind,
            };
            self.expect(&Tok::RParen, ") to close the rounding")?;
            let (op, value) = self.compared_against()?;
            return Ok(Some(Cond::Rounded { field: column, round, op, value }));
        }

        let (column, unit) = match round {
            Round::Trunc => {
                let unit = match self.literal("a quoted unit, like 'month'")? {
                    Literal::Str(u) => match Unit::parse(&u) {
                        Some(unit) => unit,
                        None => return Err(self.refuse_at(Refused::TruncUnit, at)),
                    },
                    // `date_trunc(month, ts)` reads as two columns everywhere else in this
                    // dialect, so it is a syntax error rather than a unit nobody spelled.
                    _ => return Err(self.syntax("a quoted unit, like 'month'")),
                };
                self.expect(&Tok::Comma, ", between the unit and the column")?;
                (self.name("a column name")?, unit)
            }
            Round::Day => (self.name("a column name")?, Unit::Day),
            Round::Year => (self.name("a column name")?, Unit::Year),
            Round::Number(_) => unreachable!("a number's rounding returned above"),
        };
        self.expect(&Tok::RParen, ") to close the rounding")?;
        let op_at = self.at();
        let (op, value) = self.compared_against()?;

        let bucket = self.bucket(unit, &value, matches!(round, Round::Year), op_at)?;
        Ok(Some(bucket.compare(&column, op, op_at)?))
    }

    /// The `<op> <value>` a rounding is tested by.
    ///
    /// Only the six comparisons: a rounding is a value, and `IN`, `BETWEEN` and `LIKE` over one
    /// would each be a second rewrite with its own edges to get wrong. They are refused as
    /// syntax, which points at the word that was written.
    fn compared_against(&mut self) -> Result<(&'static str, Literal)> {
        let Some(Tok::Op(op)) = self.peek() else {
            return Err(self.syntax("a comparison against the rounded value"));
        };
        let op = *op;
        self.i += 1;
        Ok((op, self.literal("a value to compare against")?))
    }

    /// The half-open range of written values that round to the one compared against.
    fn bucket(&self, unit: Unit, value: &Literal, by_year: bool, at: usize) -> Result<Bucket> {
        if by_year {
            let year = match value {
                Literal::Int(n) => {
                    i64::try_from(*n).map_err(|_| self.refuse_at(Refused::Round, at))?
                }
                _ => return Err(self.syntax("a year, like 2024")),
            };
            let day = |y| big_civil::format_date(big_civil::days_from_civil(y, 1, 1));
            // Every whole number names a year, so there is no boundary for one to miss.
            return Ok(Bucket { low: day(year), high: day(year + 1), exact: true });
        }

        let Literal::Str(written) = value else {
            return Err(self.syntax("a written date, like '2024-01-01'"));
        };
        // Read through the wider of the two spellings: a bare date is midnight here, which is
        // what it is to a `DATETIME` column as well, so one parse covers both shapes of literal.
        let Some(seconds) = big_civil::parse_datetime(written) else {
            return Err(self.syntax("a written date, like '2024-01-01'"));
        };
        let (low, high) = (big_civil::truncate(seconds, unit), big_civil::next(seconds, unit));
        // A boundary of a day or coarser lands at midnight, so it is written back as a plain
        // date - which a `DATE` column can read and a `DATETIME` takes as midnight. Below a day
        // the time of day is the answer, so it stays.
        let write = |s: i64| match unit.is_whole_days() {
            true => big_civil::format_date(big_civil::to_days(s)),
            false => big_civil::format_datetime(s),
        };
        Ok(Bucket { low: write(low), high: write(high), exact: low == seconds })
    }
}

/// The values a rounding maps into one answer: `[low, high)`, as they would be written.
struct Bucket {
    low: String,
    high: String,
    /// Whether the value compared against is itself the start of the bucket.
    ///
    /// It is what tells `= '2024-01-01'`, which every January answers, from `= '2024-01-15'`,
    /// which nothing answers because no rounding to a month ever lands mid-month.
    exact: bool,
}

impl Bucket {
    /// The condition the comparison becomes.
    ///
    /// Written out per operator rather than derived, because each is a different sentence about
    /// the same range and three of them do not mention `low` at all. `r` is the rounded value,
    /// `x` the column, and every line below is the equivalence being claimed:
    ///
    /// ```text
    /// r  = v   ->  low <= x < high      (v is a boundary; nothing otherwise)
    /// r != v   ->  NOT the above
    /// r >= v   ->  x >= low             when v is a boundary
    /// r >= v   ->  x >= high            otherwise: the first bucket at or after v starts there
    /// r  > v   ->  x >= high            a bucket strictly after v starts at high either way
    /// r  < v   ->  x < low              when v is a boundary
    /// r  < v   ->  x < high             otherwise
    /// r <= v   ->  x < high
    /// ```
    fn compare(self, column: &Name, op: &'static str, at: usize) -> Result<Cond> {
        let cmp = |op: &'static str, v: &str| Cond::Cmp {
            field: column.clone(),
            op,
            value: Literal::Str(v.to_string()),
        };
        let within = || Cond::And(Box::new(cmp(">=", &self.low)), Box::new(cmp("<", &self.high)));

        Ok(match op {
            // **Not answered as the empty set it strictly is.** There are no bind parameters in
            // this dialect, so a value off the boundary was typed by somebody who meant a value
            // on it, and an empty answer would look like a table with no January in it. The
            // refusal names the two dates that bracket what was written.
            "=" | "!=" if !self.exact => return Err(self.refuse(at)),
            "=" => within(),
            "!=" => Cond::Not(Box::new(within())),
            ">=" if self.exact => cmp(">=", &self.low),
            ">=" | ">" => cmp(">=", &self.high),
            "<" if self.exact => cmp("<", &self.low),
            "<" | "<=" => cmp("<", &self.high),
            other => unreachable!("the lexer produces no comparison but the six, not `{other}`"),
        })
    }

    fn refuse(&self, at: usize) -> crate::error::SqlError {
        crate::error::SqlError::Refused { what: Refused::Round, at }
    }
}
