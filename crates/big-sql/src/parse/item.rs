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
use crate::ast::{Agg, Cond, Item, Name, Over, Proj};
use crate::error::{Refused, Result};
use crate::lex::Tok;
use crate::scalar::Scalar;
use crate::shape::WinFunc;
use big_plan::Literal;

impl Parser<'_> {
    /// One select-list entry, and its alias.
    ///
    /// Everything but `*` goes through the expression parser, because a bare column and
    /// `round(amount / 100, 2)` differ only in how much of the tree is the identity. What comes
    /// back is the tree plus its leaves, and this decides whether the entry is answerable:
    /// **exactly one leaf**, because a projection is one plan reading one field per column and
    /// an aggregate is one plan producing one number. See [`crate::scalar::Scalar::leaves`].
    pub(super) fn item(&mut self) -> Result<Item> {
        let at = self.at();
        // **A ranking or an offset is parsed whole, here, before anything else looks at it.**
        // `row_number` and its family are not expressions and not aggregates - there is no
        // reading of `rank(x)` that means anything without the `OVER` that follows - so they are
        // taken in one piece rather than parsed as a call and repaired afterwards. An aggregate
        // window is the other way round: `sum(amount)` is a complete entry until an `OVER`
        // follows it, so that one is converted below, where the `OVER` is seen.
        if let Some(item) = self.ranking_entry()? {
            return Ok(item);
        }
        // Filled in when the aggregate carried an `-If`, which is the same clause as a trailing
        // `FILTER (WHERE ...)` and must not be written twice.
        let mut from_aggregate = None;
        let proj = if self.eat(&Tok::Star) {
            Proj::Star
        } else if self.word_is("DISTINCT") {
            return Err(self.refuse(Refused::MultiDistinct));
        } else {
            let parsed = self.expr()?;
            let written = self.src[at..self.at().min(self.src.len())].trim().to_string();
            // **Mentions of one column are one leaf.** `price * price` reads `price` once and
            // squares the value it got, so it is a projection of one field like any other -
            // what the rule is about is how many *columns* a cell would have to read, not how
            // many times the expression writes one down. Collapsed here rather than in
            // `Scalar::leaves`, which counts nodes and should go on counting nodes.
            let mut leaves = parsed.leaves;
            if leaves.windows(2).all(|w| w[0].proj == w[1].proj) {
                // The filter comes off whichever mention carried one; two mentions of an
                // aggregate cannot each carry a different `FILTER`, because the parser attaches
                // one to the item rather than to a mention.
                let filter = leaves.iter_mut().find_map(|l| l.filter.take());
                leaves.truncate(1);
                if let Some(first) = leaves.first_mut() {
                    first.filter = filter;
                }
            }
            match leaves.len() {
                1 => {
                    let leaf = leaves.remove(0);
                    from_aggregate = leaf.filter;
                    match parsed.expr.is_identity() {
                        // The column or the aggregate, untouched. Handed back as itself so that
                        // nothing downstream has to see through an identity wrapper.
                        true => leaf.proj,
                        false => {
                            Proj::Scalar { inner: Box::new(leaf.proj), expr: parsed.expr, written }
                        }
                    }
                }
                // `now()` alone is the one entry that names no column and is still an answer:
                // it reads nothing, and every row carries the same instant.
                0 => match parsed.expr {
                    Scalar::Now { unix_seconds } => Proj::Now { unix_seconds },
                    // A constant expression - `1 + 1` - has no column to be about, and a cell
                    // of it would be a row count wearing a value.
                    _ => return Err(self.refuse_at(Refused::Expression, at)),
                },
                // Two columns in one cell. A projection reads one field per column, so there is
                // no plan this could be - see the module header on the one-leaf rule.
                _ => return Err(self.refuse_at(Refused::Expression, at)),
            }
        };

        // `sum(amount) OVER (PARTITION BY country)`: the same fold, over the rows a projection
        // read rather than over a bitmap. Everything else beside an `OVER` is refused by name -
        // a window is about a row's place among others, and a bare column or a `topK` has none.
        let proj = if self.word_is("OVER") {
            if from_aggregate.is_some() {
                // A `FILTER` narrows an aggregate's own row set, and a window has none to
                // narrow - it sees the rows the projection already read.
                return Err(self.refuse(Refused::Shape));
            }
            let (func, arg) = match proj {
                Proj::Count => (WinFunc::Count, None),
                Proj::Agg { func: Agg::Sum, field } => (WinFunc::Sum, Some(field)),
                Proj::Agg { func: Agg::Min, field } => (WinFunc::Min, Some(field)),
                Proj::Agg { func: Agg::Max, field } => (WinFunc::Max, Some(field)),
                Proj::Avg(field) => (WinFunc::Avg, Some(field)),
                _ => return Err(self.refuse_at(Refused::Window, at)),
            };
            Proj::Window(Box::new(self.over(func, arg, 1, at)?))
        } else {
            proj
        };

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
impl Parser<'_> {
    /// `row_number() OVER (...)`, `lag(x, 2) OVER (...)`: the families that are only ever windows.
    ///
    /// `Ok(None)` when the select list entry is not one of them, which is every entry this
    /// grammar had before windows existed - so nothing that parsed before reaches any of this.
    ///
    /// A name from this list *without* an `OVER` is refused rather than left to fall through to
    /// `unsupported_call`. `rank(x)` is not a function nobody defined; it is a window missing the
    /// clause that says which rows it ranks, and that is the sentence worth printing.
    fn ranking_entry(&mut self) -> Result<Option<Item>> {
        let at = self.at();
        let Some(name) = self.word() else { return Ok(None) };
        let Some(func) = ranking_of(name) else { return Ok(None) };
        // `rank` and `lag` are legal column names right up until a `(` follows them, exactly as
        // `count` and `sum` are.
        if self.t.get(self.i + 1).map(|t| &t.tok) != Some(&Tok::LParen) {
            return Ok(None);
        }
        self.i += 2;

        // The argument, and the offset that rides beside it. `ntile(4)` writes its bucket count
        // where the others write a column, which is why the two are read together.
        let (arg, offset) = match func {
            WinFunc::NTile => (None, Some(self.window_number()?)),
            f if f.needs_arg() => {
                let column = self.name("a column for the window function to read")?;
                let offset = match self.eat(&Tok::Comma) {
                    true => Some(self.window_number()?),
                    false => None,
                };
                (Some(column), offset)
            }
            _ => (None, None),
        };
        self.expect(&Tok::RParen, ") to close the window function")?;

        // `nth_value(x, 2)` counts from one and `lag(x, 2)` steps back two, so one is the
        // default for both - and `lag(x, 0)` is this row, which is a way of writing `x`.
        let offset = offset.unwrap_or(1);
        if offset == 0 && matches!(func, WinFunc::NTile | WinFunc::NthValue) {
            return Err(self.refuse_at(Refused::Window, at));
        }

        if !self.word_is("OVER") {
            return Err(self.refuse_at(Refused::Window, at));
        }
        let over = self.over(func, arg, offset, at)?;
        let alias =
            if self.eat_word("AS") { Some(self.bare_ident("a name after AS")?) } else { None };
        Ok(Some(Item { proj: Proj::Window(Box::new(over)), filter: None, alias, at }))
    }

    /// The `OVER (...)` clause: which rows the function sees, and in what order.
    ///
    /// Refuses a frame at the keyword rather than accepting and ignoring one, and takes opposite
    /// answers about an `ORDER BY` from the two families - see [`Refused::WindowFrame`], which is
    /// where that reasoning is written down.
    fn over(&mut self, func: WinFunc, arg: Option<Name>, offset: u32, at: usize) -> Result<Over> {
        self.expect_word("OVER", "OVER after a window function")?;
        // `OVER w`, naming a `WINDOW` clause this grammar does not have.
        if !matches!(self.peek(), Some(&Tok::LParen)) {
            return Err(self.refuse(Refused::WindowName));
        }
        self.i += 1;

        let mut partition = Vec::new();
        if self.eat_word("PARTITION") {
            self.expect_word("BY", "BY after PARTITION")?;
            partition.push(self.name("a column to partition by")?);
            while self.eat(&Tok::Comma) {
                partition.push(self.name("a column to partition by")?);
            }
        }

        let mut order = Vec::new();
        if self.eat_word("ORDER") {
            self.expect_word("BY", "BY after ORDER")?;
            loop {
                let column = self.name("a column to order the window by")?;
                let desc = match () {
                    () if self.eat_word("DESC") => true,
                    () => {
                        self.eat_word("ASC");
                        false
                    }
                };
                order.push((column, desc));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }

        // A frame, refused at the word. Checked before the ordering rules below so that
        // `sum(x) OVER (ORDER BY t ROWS BETWEEN ...)` is named as the frame it wrote rather than
        // as the ordering that implies one.
        if matches!(self.word(), Some(w) if matches!(w.to_ascii_uppercase().as_str(),
            "ROWS" | "RANGE" | "GROUPS" | "EXCLUDE"))
        {
            return Err(self.refuse(Refused::WindowFrame));
        }
        self.expect(&Tok::RParen, ") to close OVER")?;

        // **The two families take opposite answers, and each has its own reason.** A ranking or
        // an offset with nothing to order by is not a ranking - `row_number()` over an unordered
        // partition is a number nobody can predict or reproduce. An aggregate *with* an ordering
        // is the running total, which is a frame this surface does not have.
        match (func.is_aggregate(), order.is_empty()) {
            (false, true) => return Err(self.refuse_at(Refused::WindowFrame, at)),
            (true, false) => return Err(self.refuse_at(Refused::WindowFrame, at)),
            _ => {}
        }
        Ok(Over { func, arg, offset, partition, order, at })
    }

    /// A whole number written inside a window function: `lag(x, 2)`, `ntile(4)`.
    fn window_number(&mut self) -> Result<u32> {
        match self.peek() {
            Some(Tok::Num(Literal::Int(n))) => {
                let n = *n;
                self.i += 1;
                u32::try_from(n).map_err(|_| self.syntax("a small whole number"))
            }
            _ => Err(self.syntax("a whole number")),
        }
    }
}

/// Whether a call names a regular expression, in every spelling the two dialects write them in.
///
/// **One list, two callers**: the select list reaches it through `unsupported_call`, and a
/// `WHERE` reaches it directly - because a `WHERE` refuses *known* scalar functions by name and
/// these are not known ones, so without this they would fall through to a syntax error about a
/// bracket. A named list rather than a fallthrough either way: somebody who wrote
/// `match(c, '^a')` asked a real question, and what they need is the pattern language that *is*
/// here rather than a sentence about there being no such function.
pub(super) fn is_regex_call(name: &str) -> bool {
    const REGEXES: [&str; 9] = [
        "match",
        "extract",
        "extractAll",
        "replaceRegexpOne",
        "replaceRegexpAll",
        "regexp_extract",
        "regexp_replace",
        "regexp_like",
        "regexp_matches",
    ];
    REGEXES.iter().any(|c| name.eq_ignore_ascii_case(c))
}

/// The window functions that are *only* windows, by the name every dialect writes them under.
///
/// `sum`, `avg`, `count`, `min` and `max` are deliberately absent: those are aggregates until an
/// `OVER` follows, and are converted where that `OVER` is read. Splitting the two lists is what
/// keeps `sum(amount)` parsing exactly as it always did.
fn ranking_of(name: &str) -> Option<WinFunc> {
    const NAMES: [(&str, WinFunc); 13] = [
        ("row_number", WinFunc::RowNumber),
        ("rank", WinFunc::Rank),
        ("dense_rank", WinFunc::DenseRank),
        ("ntile", WinFunc::NTile),
        ("percent_rank", WinFunc::PercentRank),
        ("cume_dist", WinFunc::CumeDist),
        ("lag", WinFunc::Lag),
        ("lead", WinFunc::Lead),
        ("first_value", WinFunc::FirstValue),
        ("last_value", WinFunc::LastValue),
        ("nth_value", WinFunc::NthValue),
        // A difference against two of a column's values, so it reads a column and needs the
        // order the difference is taken along - which puts it in this list rather than among
        // the aggregates, whose `OVER` must *not* carry one.
        ("runningDifference", WinFunc::RunningDifference),
        ("running_difference", WinFunc::RunningDifference),
    ];
    NAMES.iter().find(|(n, _)| name.eq_ignore_ascii_case(n)).map(|(_, f)| *f)
}

pub(super) struct Aggregate {
    pub proj: Proj,
    pub filter: Option<Cond>,
}

/// Which aggregate a name spells, before the bracket has been seen.
enum Which {
    Count,
    Avg,
    Agg(Agg),
    /// `uniq` and its seven approximate cousins.
    ///
    /// **All eight map to the exact count**, because `Distinct` here walks each fragment once and
    /// counts overlaps without building them - there is nothing an approximation would buy. A
    /// client porting from ClickHouse gets a different number where its sketch was wrong.
    ///
    /// `approx_count_distinct` is on the list for the same reason the other seven are, and not
    /// because Doris is owed a spelling: a name that promises an approximation is answered
    /// exactly, which is a promise kept rather than broken. It is deliberately *not* on
    /// `unsupported_call`'s bitmap list - that list is for names with no answer here, and this
    /// one has the same answer as `uniq`.
    Uniq,
    TopKeys,
    /// A quantile, with the level a named spelling fixes - `median` is `quantile(0.5)`.
    Quantile(Option<u32>),
}

impl Which {
    fn of(name: &str) -> Option<Self> {
        const UNIQ: [&str; 8] = [
            "uniq",
            "uniqExact",
            "uniqCombined",
            "uniqCombined64",
            "uniqHLL12",
            "uniqTheta",
            "approx_count_distinct",
            "approxCountDistinct",
        ];
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
pub(super) fn unsupported_call(name: &str) -> Refused {
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
    /// Bitmaps named as values, in both dialects that spell them.
    ///
    /// **The one list here whose members all have answers already.** The others name questions
    /// this engine cannot fold; these name questions it folds under a different word, because
    /// the bitmaps are the storage rather than a value to pass between calls. Listing both
    /// spellings is the point: a query arrives here having been written against Doris or
    /// ClickHouse, and what its author needs is the local word, not the news that bitmaps are
    /// unsupported in a bitmap database.
    const BITMAPS: [&str; 23] = [
        "to_bitmap",
        "groupBitmapState",
        "groupBitmap",
        "bitmap_and",
        "bitmap_or",
        "bitmap_xor",
        "bitmap_andnot",
        "bitmapAnd",
        "bitmapOr",
        "bitmapXor",
        "bitmapAndnot",
        "bitmap_count",
        "bitmap_cardinality",
        "bitmapCardinality",
        "bitmap_contains",
        "bitmap_union",
        "bitmap_subset_in_range",
        "bitmapSubsetInRange",
        "intersect_count",
        "orthogonal_bitmap_union_count",
        "bsi_sum",
        "bsi_range",
        "bsi_topk",
    ];

    /// Sketches, and the combinators that carry one between queries.
    ///
    /// Deliberately *not* the `uniq*` family, which is answered exactly - see `Which::Uniq`.
    /// What is here are the names that ask for a sketch as a *value*: a state to merge later,
    /// or a column holding one.
    const SKETCHES: [&str; 8] = [
        "uniqState",
        "uniqMerge",
        "quantileState",
        "quantileMerge",
        "hll_union_agg",
        "hll_cardinality",
        "hll_hash",
        "approx_top_k",
    ];

    /// The row-sequence family: one that has a local spelling, three that need a frame.
    ///
    /// `runningDifference` is deliberately absent - it is a window function here, and putting
    /// it on this list would refuse the thing that works.
    const SEQUENCES: [&str; 6] = [
        "neighbor",
        "sequenceMatch",
        "sequence_match",
        "windowFunnel",
        "window_funnel",
        "retention",
    ];

    if SEQUENCES.iter().any(|c| name.eq_ignore_ascii_case(c)) {
        return Refused::SequenceFunction;
    }
    /// The array family, whose members all have a spelling over a keyed column.
    ///
    /// The `array*` prefix is checked rather than listed, because it composes with every
    /// higher-order name there is and a list would be that many entries saying one thing. What
    /// is listed are the ones that do not carry it.
    ///
    /// **`groupArray` is deliberately not here.** It asks for a *row* - a list of values as one
    /// cell - which is the refusal `Refused::Unsupported` already carries and the one the
    /// corpus header names it under. `explode` and `unnest` are the `ARRAY JOIN` clause and are
    /// refused where that clause is.
    const ARRAYS: [&str; 4] = ["has", "hasAll", "hasAny", "indexOf"];
    if name.len() > 5 && name[..5].eq_ignore_ascii_case("array")
        || ARRAYS.iter().any(|c| name.eq_ignore_ascii_case(c))
    {
        return Refused::ArrayFunction;
    }
    // Checked before the casts, because `to_bitmap` reads like a conversion and is not one:
    // what it names is already how the fact was written, so the sentence it needs is the
    // mapping rather than the one about representations that do not convert.
    if BITMAPS.iter().any(|c| name.eq_ignore_ascii_case(c)) {
        return Refused::BitmapFunction;
    }
    // A `-State`/`-Merge` suffix on anything, plus the sketch names that carry no suffix. The
    // suffix is checked rather than listed because it composes with every aggregate there is,
    // and a list would be that many entries to say one thing.
    let suffixed = ["State", "Merge", "MergeState"].iter().any(|sfx| {
        name.len() > sfx.len() && name[name.len() - sfx.len()..].eq_ignore_ascii_case(sfx)
    });
    if suffixed || SKETCHES.iter().any(|c| name.eq_ignore_ascii_case(c)) {
        return Refused::Sketch;
    }
    if is_regex_call(name) {
        return Refused::Regex;
    }
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
