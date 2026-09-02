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

//! One entry of the select list: a star, a column, or an aggregate with its `FILTER` and alias.

use super::Parser;
use crate::ast::{Agg, Cond, Item, Proj, TimeOp};
use crate::error::{Refused, Result};
use crate::lex::Tok;
use big_plan::Literal;

impl Parser<'_> {
    /// One select-list entry, and its alias.
    pub(super) fn item(&mut self) -> Result<Item> {
        let at = self.at();
        // Filled in when the aggregate carried an `-If`, which is the same clause as a trailing
        // `FILTER (WHERE ...)` and must not be written twice.
        let mut from_aggregate = None;
        let proj = if self.eat(&Tok::Star) {
            Proj::Star
        } else if self.word_is("DISTINCT") {
            return Err(self.refuse(Refused::MultiDistinct));
        } else if self.word_is("CASE") {
            // Before the name is read, because `CASE` is a keyword rather than a column and
            // reading it as one dies at `WHEN` with "expected FROM" - a syntax error about
            // perfectly good SQL, which is the failure this crate exists to avoid.
            return Err(self.refuse(Refused::Case));
        } else {
            let name = self.name("a column or an aggregate")?;
            // A qualified name is a column of a named table, never a function: `a.count(*)`
            // is not a spelling of anything.
            let agg = match name.qualifier {
                Some(_) => None,
                None => self.aggregate(&name.column)?,
            };
            match agg {
                Some(a) => {
                    from_aggregate = a.filter;
                    a.proj
                }
                // The scalar calls, tried before the refusal below. They are not aggregates -
                // they fold nothing and read no more than the column already read - so they do
                // not belong in `Which`, whose whole list is things that produce one number
                // from many records.
                None if self.peek() == Some(&Tok::LParen) && self.scalar_name(&name.column) => {
                    self.scalar(&name.column)?
                }
                // A bare name followed by `(` is a function this dialect does not have. Which
                // refusal it earns depends on what was asked for: an aggregate with no fold
                // behind it, a conversion between representations that do not convert, and a
                // choice per record all have their own sentence, and a client that wrote one of
                // them needs the reason rather than "no expressions here".
                None if self.peek() == Some(&Tok::LParen) => {
                    return Err(self.refuse_at(unsupported_call(&name.column), at))
                }
                // An operator or a `*` after a bare column is arithmetic, which this dialect
                // does not evaluate. Caught here so it is refused as what it is rather than as
                // "expected FROM".
                None if matches!(self.peek(), Some(Tok::Op(_)) | Some(Tok::Star)) => {
                    return Err(self.refuse(Refused::Expression))
                }
                None => Proj::Column(name),
            }
        };

        if self.word_is("OVER") {
            return Err(self.refuse(Refused::Window));
        }

        // `FILTER (WHERE ...)` narrows one aggregate. On a star or a bare column there is no
        // aggregate to narrow: `WHERE` is where that condition goes, and a second spelling of
        // it would be a second place for a client to contradict itself.
        let filter = if self.word_is("FILTER") {
            if matches!(proj, Proj::Star | Proj::Column(_)) || from_aggregate.is_some() {
                // A star or a bare column has no aggregate to narrow, and an `-If` already
                // narrowed this one - a second spelling of the same clause is a second place
                // for a client to contradict itself.
                return Err(self.refuse(Refused::Shape));
            }
            self.i += 1;
            self.expect(&Tok::LParen, "( after FILTER")?;
            self.expect_word("WHERE", "WHERE inside FILTER")?;
            let cond = self.cond()?;
            self.expect(&Tok::RParen, ") to close FILTER")?;
            Some(cond)
        } else {
            from_aggregate
        };

        let alias =
            if self.eat_word("AS") { Some(self.bare_ident("a name after AS")?) } else { None };
        Ok(Item { proj, filter, alias, at })
    }

    /// Whether a name is one of the scalar calls, which decides only whether [`Self::scalar`]
    /// is asked. The refusal for everything else stays where it was.
    pub(super) fn scalar_name(&self, name: &str) -> bool {
        ["now", "toDate", "date_trunc", "toStartOfInterval"]
            .iter()
            .any(|c| name.eq_ignore_ascii_case(c))
    }

    /// `now()`, `toDate(<column>)`, `date_trunc('<unit>', <column>)`, with the name consumed and
    /// the `(` still ahead.
    ///
    /// **These read no more than the plan already reads.** `toDate` and `date_trunc` round a
    /// value the projection was going to return anyway, applied where a decimal has its point
    /// put back; `now()` reads nothing at all. That is why they can exist in a surface with no
    /// expression evaluator, and it is also the boundary: a scalar call in a `WHERE` would have
    /// to be computed per record before the filter, and there is nothing here that could.
    fn scalar(&mut self, name: &str) -> Result<Proj> {
        // The name is already consumed - `item` reads it before it can tell a column from a
        // call - so what is ahead is the bracket.
        let at = self.at();
        self.expect(&Tok::LParen, "( after the function name")?;

        if name.eq_ignore_ascii_case("now") {
            self.expect(&Tok::RParen, ") after now(")?;
            return Ok(Proj::Now { unix_seconds: self.now });
        }
        if name.eq_ignore_ascii_case("toDate") {
            let field = self.name("a column")?;
            self.expect(&Tok::RParen, ") after the column")?;
            return Ok(Proj::TimeOf { op: TimeOp::ToDate, field });
        }

        // `date_trunc(unit, column)` and ClickHouse's `toStartOfInterval(column, INTERVAL 1
        // unit)` are the same question; only the first is read, and the second is refused by
        // name below so that somebody who wrote it is told which spelling this dialect takes
        // rather than that the function does not exist.
        if !name.eq_ignore_ascii_case("date_trunc") {
            return Err(self.refuse_at(Refused::Interval, at));
        }
        let unit = match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.i += 1;
                s
            }
            // The unit is a quoted string, not a bare word: `date_trunc(month, ts)` reads as two
            // columns everywhere else in this dialect, and accepting it here would make `month`
            // sometimes a column and sometimes a keyword.
            _ => return Err(self.syntax("a quoted unit, like 'month'")),
        };
        let Some(unit) = big_civil::Unit::parse(&unit) else {
            return Err(self.refuse_at(Refused::TruncUnit, at));
        };
        self.expect(&Tok::Comma, ", after the unit")?;
        let field = self.name("a column")?;
        self.expect(&Tok::RParen, ") after the column")?;
        Ok(Proj::TimeOf { op: TimeOp::Trunc(unit), field })
    }

    /// One parsed aggregate: what it measures, and the condition an `-If` narrowed it by.
    ///
    /// The two travel together because ClickHouse's `-If` combinator and standard SQL's
    /// `FILTER (WHERE ...)` are the same clause under two spellings, and both end up in the
    /// same place - this entry's own row set. Accepting both and keeping one representation is
    /// what stops them from drifting.
    ///
    /// Returns `None` when `name` is not an aggregate at all, which is how [`Parser::item`]
    /// tells a column from a function without a reserved word list.
    pub(super) fn aggregate(&mut self, name: &str) -> Result<Option<Aggregate>> {
        // `countIf`, `sumIf`, `avgIf`, `uniqIf`: the same aggregate over its own condition.
        let (stem, iff) = match name.len() > 2 && name[name.len() - 2..].eq_ignore_ascii_case("if")
        {
            true => (&name[..name.len() - 2], true),
            false => (name, false),
        };
        let Some(which) = Which::of(stem) else { return Ok(None) };
        // `count` and `sum` are legal column names right up until a `(` follows them.
        if self.peek() != Some(&Tok::LParen) {
            return Ok(None);
        }
        self.i += 1;

        // `topK(10)(country)`: ClickHouse writes the count as a parameter rather than an
        // argument, so the call has two bracket pairs. `topK(country)` takes the default.
        if let Which::TopKeys = which {
            if iff {
                return Err(self.refuse(Refused::Shape));
            }
            return self.top_keys().map(Some);
        }
        if let Which::Quantile(default) = which {
            if iff {
                return Err(self.refuse(Refused::Shape));
            }
            return self.quantile(default).map(Some);
        }

        let proj = match which {
            Which::Agg(func) => Proj::Agg { func, field: self.name("a column to aggregate")? },
            Which::Avg => Proj::Avg(self.name("a column to average")?),
            Which::Uniq => Proj::CountDistinct(self.name("a column to count the values of")?),
            Which::Count if iff => Proj::Count,
            Which::Count if self.eat(&Tok::Star) => Proj::Count,
            Which::Count if self.eat_word("DISTINCT") => {
                Proj::CountDistinct(self.name("a column")?)
            }
            // `count(x)` counts the records holding a value in `x`, which is a third question
            // with no plan behind it. Named rather than quietly answered as `count(*)`.
            Which::Count => return Err(self.refuse(Refused::Shape)),
            Which::TopKeys | Which::Quantile(_) => unreachable!("returned above"),
        };

        // `countIf(cond)` takes the condition alone; every other `-If` takes it after the
        // column, which is the order ClickHouse writes them in.
        let filter = if iff {
            if !matches!(proj, Proj::Count) {
                self.expect(&Tok::Comma, ", before the condition")?;
            }
            Some(self.cond()?)
        } else {
            None
        };

        self.expect(&Tok::RParen, ") to close the aggregate")?;
        Ok(Some(Aggregate { proj, filter }))
    }

    /// `quantile(p)(<column>)`, or a named one like `median(<column>)` whose level is fixed.
    ///
    /// The level is written the way ClickHouse writes it - a fraction between 0 and 1 - and
    /// kept as parts per thousand, which is exact where a float would not be and is finer than
    /// any distribution this engine holds enough records to resolve.
    fn quantile(&mut self, default: Option<u32>) -> Result<Aggregate> {
        let per_mille = match self.peek() {
            Some(Tok::Num(_)) if default.is_none() || self.at_second_bracket() => {
                let level = self.literal("a level between 0 and 1")?;
                let per_mille = match level {
                    Literal::Int(0) => 0,
                    Literal::Int(1) => 1_000,
                    Literal::Dec { units, scale } if scale <= 3 => {
                        units * 10u64.pow(3 - u32::from(scale))
                    }
                    // More than three digits is a level finer than this resolves, and a whole
                    // number other than 0 or 1 is not a fraction at all.
                    _ => return Err(self.refuse(Refused::Quantile)),
                };
                if per_mille > 1_000 {
                    return Err(self.refuse(Refused::Quantile));
                }
                self.expect(&Tok::RParen, ") after the level")?;
                self.expect(&Tok::LParen, "( and the column")?;
                per_mille as u32
            }
            _ => match default {
                Some(p) => p,
                None => return Err(self.refuse(Refused::Quantile)),
            },
        };
        let field = self.name("a column to take the quantile of")?;
        self.expect(&Tok::RParen, ") to close the quantile")?;
        Ok(Aggregate { proj: Proj::Quantile { per_mille, field }, filter: None })
    }

    /// Whether the number being looked at is a parameter rather than the column, which is what
    /// a second bracket pair after it says.
    fn at_second_bracket(&self) -> bool {
        matches!(self.t.get(self.i + 1).map(|t| &t.tok), Some(Tok::RParen))
    }

    /// `topK(n)(<column>)` or `topK(<column>)`, with the opening bracket already eaten.
    fn top_keys(&mut self) -> Result<Aggregate> {
        // A number here is the parameter and the column comes in a second bracket pair.
        if let Some(Tok::Num(Literal::Int(n))) = self.peek() {
            let n = *n;
            self.i += 1;
            self.expect(&Tok::RParen, ") after the number of keys")?;
            self.expect(&Tok::LParen, "( and the column to rank")?;
            let field = self.name("a column to rank")?;
            self.expect(&Tok::RParen, ") to close topK")?;
            return Ok(Aggregate { proj: Proj::TopKeys { n, field }, filter: None });
        }
        let field = self.name("a column to rank, or the number of keys")?;
        self.expect(&Tok::RParen, ") to close topK")?;
        Ok(Aggregate { proj: Proj::TopKeys { n: DEFAULT_TOP_K, field }, filter: None })
    }
}

/// How many keys `topK(x)` ranks when no number was written. ClickHouse's default.
const DEFAULT_TOP_K: u64 = 10;

/// One parsed aggregate and the condition an `-If` suffix gave it.
pub(super) struct Aggregate {
    pub proj: Proj,
    pub filter: Option<Cond>,
}

/// Which aggregate a name spells, before the bracket has been seen.
enum Which {
    Count,
    Avg,
    Agg(Agg),
    /// `uniq` and its five approximate cousins.
    ///
    /// **All six map to the exact count**, because `Distinct` here walks each fragment once and
    /// counts overlaps without building them - there is nothing an approximation would buy. A
    /// client porting from ClickHouse gets a different number where its sketch was wrong, which
    /// is worth saying out loud and is said in `docs/sql-plan.md`.
    Uniq,
    TopKeys,
    /// A quantile, with the level a named spelling fixes - `median` is `quantile(0.5)`.
    Quantile(Option<u32>),
}

impl Which {
    fn of(name: &str) -> Option<Self> {
        const UNIQ: [&str; 6] =
            ["uniq", "uniqExact", "uniqCombined", "uniqCombined64", "uniqHLL12", "uniqTheta"];
        Some(if name.eq_ignore_ascii_case("count") {
            Self::Count
        } else if name.eq_ignore_ascii_case("avg") {
            Self::Avg
        } else if name.eq_ignore_ascii_case("sum") {
            Self::Agg(Agg::Sum)
        } else if name.eq_ignore_ascii_case("min") {
            Self::Agg(Agg::Min)
        } else if name.eq_ignore_ascii_case("max") {
            Self::Agg(Agg::Max)
        } else if name.eq_ignore_ascii_case("topK") {
            Self::TopKeys
        } else if name.eq_ignore_ascii_case("median")
            || name.eq_ignore_ascii_case("quantileExact")
            || name.eq_ignore_ascii_case("quantile")
        {
            // `median` fixes the level; the other two take one. All three are exact here, which
            // `quantile` is not in ClickHouse.
            Self::Quantile(name.eq_ignore_ascii_case("median").then_some(500))
        } else if UNIQ.iter().any(|u| name.eq_ignore_ascii_case(u)) {
            Self::Uniq
        } else {
            return None;
        })
    }
}

/// Which refusal a function call in the select list earns.
///
/// **Named lists rather than "anything unknown is an expression".** Somebody who wrote
/// `stddevPop(amount)` asked a real question, and what they need is why bit planes cannot
/// answer it - not a sentence about there being no expression evaluator, which is true and
/// unhelpful. A name on none of these lists falls through to [`Refused::Aggregate`], whose
/// message names the aggregates that do exist, and that is the right sentence for `foo(x)` too.
fn unsupported_call(name: &str) -> Refused {
    /// Conversions between representations that do not convert.
    ///
    /// `toDate` used to be here and is a scalar call now - it rounds a temporal column to the
    /// day, which is a question the projection can answer without converting anything. What is
    /// still on this list are the conversions that would need a value to become a *different
    /// kind of thing*, which is what this engine has nowhere to do.
    /// `toDateTime` is still here: widening a day count to an instant would have to invent a
    /// time of day, and midnight is a guess rather than an answer.
    const CASTS: [&str; 8] =
        ["cast", "convert", "toInt64", "toUInt64", "toInt32", "toUInt32", "toString", "toDateTime"];
    /// Choosing between two values per record.
    const CHOICES: [&str; 5] = ["if", "multiIf", "coalesce", "nullIf", "ifNull"];

    if CASTS.iter().any(|c| name.eq_ignore_ascii_case(c)) {
        return Refused::Cast;
    }
    if CHOICES.iter().any(|c| name.eq_ignore_ascii_case(c)) {
        return Refused::Case;
    }
    // `argMin`, `stddevPop`, `corr`, `any` - and `foo`, which gets the same sentence because
    // the list of aggregates that exist is what a caller needs either way.
    Refused::Aggregate
}
