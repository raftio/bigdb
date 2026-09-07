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

//! The statement's own clauses: `FROM`, `JOIN`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`.

use super::Parser;
use crate::ast::{
    Cond, Grouping, GroupingSets, Having, HavingAgg, HavingOperand, Join, JoinKind, Order,
    OrderKey, Proj, Select, Source,
};
use crate::error::{Refused, Result, SqlError};
use crate::lex::Tok;
use crate::scalar::{Func, Scalar};

/// How many columns one `GROUP BY` may name.
///
/// **The real bound is the passes**, which the executor checks per level against
/// `lower::tuples::MAX_PASSES` - a grouping's cost is the number of combinations it walks, not
/// the number of columns it names. This exists so that the answer's arity stays a small number
/// the shape can carry in a byte, and so a statement naming twenty columns is refused at the
/// text rather than after building a frontier.
pub const MAX_GROUP_COLUMNS: usize = 4;

/// How many grouping sets one statement may name.
///
/// **A bound on the fan-out, not a taste in rollups.** Every set is its own grouping - a call
/// planned, sent to every owner and merged on its own - so the number of sets multiplied by the
/// aggregates in the select list is the number of round trips, against the one budget
/// `lower::MAX_CALLS` holds. `CUBE` over four columns is sixteen sets, which is that whole budget
/// with nothing left for a second aggregate; `CUBE` over three is eight and `ROLLUP` over four is
/// five, which are the two shapes a dashboard writes.
///
/// Checked here, at the text, so `CUBE(a,b,c,d)` is named as what it is rather than surfacing
/// three sets later as `sql_too_many_aggregates`. The `Calls` budget stays the real ceiling and
/// still fires for a statement that is under this one and asks too much of each set.
pub const MAX_GROUPING_SETS: usize = 8;
use big_plan::Literal;

impl Parser<'_> {
    // ------------------------------------------------------------------------------------
    // The statement
    // ------------------------------------------------------------------------------------

    pub(super) fn select(&mut self) -> Result<Select> {
        // `SELECT DISTINCT a` is `SELECT a ... GROUP BY a`, and not by analogy: standard SQL
        // defines the first as the second, and this engine answers it with the same plan a
        // `GROUP BY` gets - `Distinct(field=a)`, whose keys are the answer and whose counts
        // are simply not rendered. Normalising here rather than carrying a flag through the
        // lowering keeps one path to that plan instead of two that must agree.
        let distinct_at = self.at();
        let distinct = self.eat_word("DISTINCT");

        let mut items = vec![self.item()?];
        while self.eat(&Tok::Comma) {
            items.push(self.item()?);
        }

        self.expect_word("FROM", "FROM after the select list")?;
        let from = self.source("a table name")?;

        // A comma between tables is a cross join written the old way, and a cross join has no
        // key to pair records on. Refused at the token that says so.
        if self.peek() == Some(&Tok::Comma) {
            return Err(self.refuse(Refused::Joins));
        }
        let joins = self.joins()?;

        // `PREWHERE` is ClickHouse's hint to filter on a cheap column before reading the rest
        // of the row. **Here it selects exactly the set `WHERE` would**, because there is no
        // row to read: a record is a set of bits, and an intersection is an intersection
        // whichever clause spelled it. Accepted and folded in, so a statement ported from
        // ClickHouse answers rather than failing on a word that changes nothing.
        let pre = if self.eat_word("PREWHERE") { Some(self.cond()?) } else { None };
        let filter = if self.eat_word("WHERE") { Some(self.cond()?) } else { None };
        let filter = match (pre, filter) {
            (Some(a), Some(b)) => Some(Cond::And(Box::new(a), Box::new(b))),
            (Some(one), None) | (None, Some(one)) => Some(one),
            (None, None) => None,
        };

        let group_at = self.at();
        let mut grouping_sets = None;
        let group_by = if self.eat_word("GROUP") {
            self.expect_word("BY", "BY after GROUP")?;
            // `GROUPING SETS` is the whole clause rather than a term of it, so it forks here
            // instead of inside `grouping`: what follows is a list of lists, and a bare column
            // among them is the one-column set rather than a column of a bigger one.
            if self.word_is("GROUPING") {
                let (by, sets) = self.grouping_sets(group_at)?;
                grouping_sets = Some(sets);
                by
            } else {
                let mut by = vec![self.grouping()?];
                while self.eat(&Tok::Comma) {
                    by.push(self.grouping()?);
                }
                if by.len() > MAX_GROUP_COLUMNS {
                    return Err(self.refuse_at(Refused::Shape, group_at));
                }
                // `WITH ROLLUP` / `WITH CUBE`, which name a list of subsets of what was just
                // read. Unambiguous here: the other `WITH` this grammar has follows `LIMIT`,
                // which cannot appear before a `GROUP BY`.
                if self.eat_word("WITH") {
                    let at = self.at();
                    let of = if self.eat_word("ROLLUP") {
                        rollup(by.len())
                    } else if self.eat_word("CUBE") {
                        cube(by.len())
                    } else if self.word_is("TOTALS") {
                        return Err(self.refuse_at(Refused::WithTotals, at));
                    } else {
                        return Err(self.syntax("ROLLUP, CUBE or TOTALS after WITH"));
                    };
                    if of.len() > MAX_GROUPING_SETS {
                        return Err(self.refuse_at(Refused::GroupingSets, at));
                    }
                    grouping_sets = Some(GroupingSets { of, at });
                }
                by
            }
        } else {
            Vec::new()
        };

        let group_by = match (distinct, group_by.is_empty()) {
            (false, _) => group_by,
            // Both spellings of the same grouping, and no way to tell which one the writer
            // meant if they disagree. Refusing beats picking one.
            (true, false) => return Err(self.refuse_at(Refused::Shape, distinct_at)),
            // `SELECT DISTINCT a, b` is the same grouping `GROUP BY a, b` is, which is what
            // standard SQL says it is.
            (true, true) => {
                let mut by = Vec::new();
                for item in &items {
                    match &item.proj {
                        Proj::Column(name) => {
                            by.push(Grouping { name: name.clone(), bucket: None, at: item.at })
                        }
                        // `SELECT DISTINCT date_trunc('month', ts)` is the distinct months, and
                        // is the same grouping the written-out form is - so it reads as one.
                        Proj::Scalar { .. } => match (item.leaf(), bucket_of(item.apply())) {
                            (Proj::Column(name), Some(unit)) => by.push(Grouping {
                                name: name.clone(),
                                bucket: Some(unit),
                                at: item.at,
                            }),
                            _ => return Err(self.refuse_at(Refused::GroupExpression, item.at)),
                        },
                        // `SELECT DISTINCT count(*)`, `SELECT DISTINCT *`: distinct over
                        // something that is already one value, or over identities that are
                        // already distinct.
                        _ => return Err(self.refuse_at(Refused::Shape, item.at)),
                    }
                }
                if by.len() > MAX_GROUP_COLUMNS {
                    return Err(self.refuse_at(Refused::MultiDistinct, items[MAX_GROUP_COLUMNS].at));
                }
                by
            }
        };

        let having = if self.eat_word("HAVING") { Some(self.having()?) } else { None };

        let order_by = if self.eat_word("ORDER") {
            self.expect_word("BY", "BY after ORDER")?;
            Some(self.order()?)
        } else {
            None
        };

        let mut with_ties = false;
        let limit = if self.eat_word("LIMIT") {
            let n = match self.peek() {
                Some(Tok::Num(Literal::Int(n))) => {
                    let n = *n;
                    self.i += 1;
                    n
                }
                _ => return Err(self.syntax("a whole number after LIMIT")),
            };
            if self.eat_word("WITH") {
                self.expect_word("TIES", "TIES after WITH")?;
                if order_by.is_none() {
                    // Nothing to tie on. Refused rather than read as a plain limit, which would
                    // answer a different question quietly.
                    return Err(self.refuse(Refused::Order));
                }
                with_ties = true;
            }
            Some(n)
        } else {
            None
        };

        // Accepted into the statement rather than refused here: an offset over a list of
        // groups is applied to an answer this surface has already materialised in full, and an
        // offset over a record listing is not. Only the lowering knows which one this is.
        let offset = if self.eat_word("OFFSET") {
            match self.peek() {
                Some(Tok::Num(Literal::Int(n))) => {
                    let n = *n;
                    self.i += 1;
                    Some(n)
                }
                _ => return Err(self.syntax("a whole number after OFFSET")),
            }
        } else {
            None
        };

        // Last, because it is about the bytes rather than the answer - and because ClickHouse
        // puts it last.
        let format = self.format()?;

        Ok(Select {
            items,
            from,
            joins,
            filter,
            group_by,
            grouping_sets,
            having,
            order_by,
            limit,
            with_ties,
            offset,
            format,
        })
    }

    /// One table in `FROM`, with the alias the rest of the statement calls it by.
    pub(super) fn source(&mut self, want: &'static str) -> Result<Source> {
        let (database, table) = self.table_ref(want)?;
        // `AS` is optional in SQL and `FROM tx a` is the common spelling. A word here is an
        // alias unless it is a keyword that continues the statement - a table called `where`
        // has to be quoted, which is true of every dialect.
        let alias = if self.eat_word("AS") {
            Some(self.bare_ident("a name after AS")?)
        } else if self.word().is_some() && !self.at_clause_keyword() {
            Some(self.bare_ident("an alias")?)
        } else {
            None
        };
        Ok(Source { database, table, alias })
    }

    /// Whether the current word begins a clause rather than being an alias.
    pub(super) fn at_clause_keyword(&self) -> bool {
        const KEYWORDS: [&str; 21] = [
            "JOIN",
            "INNER",
            "LEFT",
            "RIGHT",
            "FULL",
            "CROSS",
            "NATURAL",
            "ON",
            "USING",
            "PREWHERE",
            "WHERE",
            "GROUP",
            "HAVING",
            "ORDER",
            "LIMIT",
            "OFFSET",
            "UNION",
            "INTERSECT",
            "EXCEPT",
            "FORMAT",
            // Both trailing clauses have to be here, not just the one that came first. Without
            // it `FROM t SETTINGS max_result_rows = 5` reads `SETTINGS` as the table's alias and
            // then fails on the key - and it fails *only* when nothing else follows the table,
            // so `FORMAT TSV SETTINGS ...` would go on working and hide it. A table called
            // `settings` now has to be quoted, which is what this list costs and what it costs
            // for every other word on it.
            "SETTINGS",
        ];
        KEYWORDS.iter().any(|k| self.word_is(k))
    }

    /// `[INNER] JOIN <source> ON <name> = <name>`, as many times as they are written.
    ///
    /// **The refusals here are the shape of the engine, not a stage of implementation.** An
    /// outer join has to produce a row for a record with no partner, and what this engine
    /// answers about a join is arithmetic over per-key counts - there is no row to null out
    /// half of. A `USING` clause names one column for both tables, which cannot be written
    /// when each side keeps its own dictionary.
    ///
    /// The two lists below are two different mistakes, and they used to be one. An outer join
    /// names a key and asks for the records that have no partner under it; a cross join and a
    /// natural join name **no key at all** - which is the same thing a comma between tables is,
    /// and it already has its own refusal saying so. Sending them to [`Refused::OuterJoin`] meant
    /// somebody who wrote `CROSS JOIN` was told about nulling out half a row, which is not what
    /// they asked for and not why it was refused.
    ///
    /// **Nothing here bounds how many there are.** Whether several joins are one star around
    /// one key is a question about the columns they name, and the parser has no scope to ask it
    /// in - so it is the lowering that says so, where the sentence can name the table that
    /// would have needed two keys.
    pub(super) fn joins(&mut self) -> Result<Vec<Join>> {
        let mut out = Vec::new();
        while let Some(join) = self.join()? {
            out.push(join);
        }
        Ok(out)
    }

    /// One `JOIN`, or nothing when the next word does not begin one.
    fn join(&mut self) -> Result<Option<Join>> {
        let at = self.at();
        for kw in ["CROSS", "NATURAL"] {
            if self.word_is(kw) {
                return Err(self.refuse(Refused::Joins));
            }
        }
        // **An outer join names a key and asks which sides have to hold it**, which is a fact
        // about the key space rather than a second kind of answer - so it is one word here and
        // one flag per side in the shape. `OUTER` is noise in all three spellings: it says
        // what `LEFT`, `RIGHT` and `FULL` already say.
        let kind = if self.eat_word("LEFT") {
            self.eat_word("OUTER");
            JoinKind::Left
        } else if self.eat_word("RIGHT") {
            self.eat_word("OUTER");
            JoinKind::Right
        } else if self.eat_word("FULL") {
            self.eat_word("OUTER");
            JoinKind::Full
        } else {
            self.eat_word("INNER");
            JoinKind::Inner
        };
        // After one of those three words a `JOIN` is the only thing that can follow, so it is
        // expected rather than peeked at: returning `None` here would leave the word eaten and
        // the clause silently skipped.
        if matches!(kind, JoinKind::Inner) {
            if !self.eat_word("JOIN") {
                return Ok(None);
            }
        } else {
            self.expect_word("JOIN", "JOIN after the kind of join")?;
        }
        let source = self.source("a table name after JOIN")?;
        if self.word_is("USING") {
            return Err(self.refuse(Refused::JoinOn));
        }
        self.expect_word("ON", "ON after the joined table")?;

        let left = self.name("a column on one side of the join")?;
        match self.peek() {
            Some(Tok::Op("=")) => self.i += 1,
            _ => return Err(self.refuse(Refused::JoinOn)),
        }
        let right = self.name("a column on the other side of the join")?;
        // A second condition is a join on a composite key, which is a grouping over a pair of
        // columns this index never stored - the same refusal `GROUP BY a, b` gets.
        if self.word_is("AND") || self.word_is("OR") {
            return Err(self.refuse(Refused::JoinOn));
        }
        Ok(Some(Join { kind, source, left, right, at }))
    }

    pub(super) fn order(&mut self) -> Result<Order> {
        let at = self.at();
        // `count` is only the aggregate when a `(` follows it; a column called `count` is still
        // a column, here as in the select list. `aggregate` makes that distinction once, so an
        // ordering and a select-list entry cannot come to disagree about it.
        let name = self.name("a column, an alias, or an aggregate")?;
        let agg = match name.qualifier {
            Some(_) => None,
            None => self.aggregate(&name.column)?,
        };
        // An `-If` here would name a number by repeating its condition; the alias the select
        // list gave it names the same number without the repetition, and cannot disagree.
        let key = match agg.map(|a| (a.proj, a.filter.is_some())) {
            Some((_, true)) => return Err(SqlError::Refused { what: Refused::Order, at }),
            Some((Proj::Count, _)) => OrderKey::Count,
            Some((Proj::Agg { func, field }, _)) => OrderKey::Agg { func, field },
            Some((Proj::Avg(field), _)) => OrderKey::Avg(field),
            // `ORDER BY count(DISTINCT x)` orders by a number that is the whole answer rather
            // than one of its rows.
            Some(_) => return Err(SqlError::Refused { what: Refused::Order, at }),
            // A `(` after a name that is not one of the five aggregates is a function this
            // dialect does not have, which is the select list's refusal and not an ordering's.
            None if self.peek() == Some(&Tok::LParen) => {
                return Err(self.refuse(Refused::Expression))
            }
            None => OrderKey::Name(name),
        };

        let desc = if self.eat_word("DESC") {
            true
        } else {
            self.eat_word("ASC");
            false
        };
        if self.peek() == Some(&Tok::Comma) {
            // Two orderings would be a sort, and what this engine has is a ranking.
            return Err(SqlError::Refused { what: Refused::Order, at });
        }
        Ok(Order { key, desc, at })
    }

    /// `HAVING <comparison> [AND|OR <comparison>]*`, with `NOT` and brackets.
    ///
    /// **The same four-function chain [`Parser::cond`] is**, and deliberately so: two grammars
    /// that read alike are two grammars a reader learns once. What differs is the leaf — a
    /// `WHERE` term names a column and a `HAVING` term names an aggregate — and that difference
    /// is the whole reason they are separate types. See [`Having`].
    ///
    /// **Which** aggregate is legal is not decided here: whether the number named is one the
    /// answer carries is a question about the select list, and the lowering asks it. This only
    /// refuses what is not an aggregate at all, so that `HAVING country = 'GB'` is named as a
    /// `HAVING` this surface does not take rather than as a syntax error somewhere after it.
    pub(super) fn having(&mut self) -> Result<Having> {
        self.enter()?;
        let mut left = self.having_conj()?;
        while self.eat_word("OR") {
            left = Having::Or(Box::new(left), Box::new(self.having_conj()?));
        }
        self.leave();
        Ok(left)
    }

    fn having_conj(&mut self) -> Result<Having> {
        self.enter()?;
        let mut left = self.having_neg()?;
        while self.eat_word("AND") {
            left = Having::And(Box::new(left), Box::new(self.having_neg()?));
        }
        self.leave();
        Ok(left)
    }

    fn having_neg(&mut self) -> Result<Having> {
        self.enter()?;
        let out = if self.eat_word("NOT") {
            Having::Not(Box::new(self.having_neg()?))
        } else if self.peek() == Some(&Tok::LParen) {
            // A `(` beginning a select is a subquery wherever it appears, and saying so here is
            // what keeps it from dying as "expected an aggregate".
            if self.word_at_is(1, "SELECT") {
                return Err(self.refuse(Refused::Subquery));
            }
            self.i += 1;
            let inner = self.having()?;
            self.expect(&Tok::RParen, ") to close the HAVING")?;
            inner
        } else {
            self.having_cmp()?
        };
        self.leave();
        Ok(out)
    }

    /// One comparison: `<operand> <op> <operand>`.
    ///
    /// Both sides go through the same reader, which is what makes `HAVING sum(paid) > sum(due)`
    /// fall out rather than needing a case of its own — a comparison does not care which side
    /// an aggregate is on, and neither does the group it is evaluated against.
    fn having_cmp(&mut self) -> Result<Having> {
        let at = self.at();
        let left = self.having_operand(at)?;

        let Some(Tok::Op(op)) = self.peek() else {
            return Err(self.syntax("a comparison after the aggregate"));
        };
        let op = *op;
        self.i += 1;

        let right = self.having_operand(self.at())?;
        Ok(Having::Cmp { left, op, right, at })
    }

    /// An aggregate, or a value to compare one against.
    fn having_operand(&mut self, at: usize) -> Result<HavingOperand> {
        // A literal is a value and nothing else - and any literal, not only a whole number: a
        // threshold on a decimal field is written the way the field is written, and
        // `Shape::resolve` converts it with the same code a `WHERE` comparison goes through.
        if !matches!(self.peek(), Some(Tok::Word(_)) | Some(Tok::Quoted(_))) {
            return Ok(HavingOperand::Value(
                self.literal("a value to compare the aggregate against")?,
            ));
        }
        let Ok(name) = self.bare_ident("an aggregate") else {
            return Err(self.refuse_at(Refused::Having, at));
        };
        Ok(HavingOperand::Agg(match self.aggregate(&name)?.map(|a| (a.proj, a.filter.is_some())) {
            // As in `ORDER BY`: a filtered aggregate is named by its alias, not by repeating
            // the condition that made it.
            Some((_, true)) => return Err(self.refuse_at(Refused::Having, at)),
            Some((Proj::Count, _)) => HavingAgg::Count,
            Some((Proj::Agg { func, field }, _)) => HavingAgg::Agg { func, field },
            Some((Proj::Avg(field), _)) => HavingAgg::Avg(field),
            // `count(DISTINCT x)` in a HAVING is a second grouping inside the first, and a
            // bare column is a predicate on a value no group holds.
            Some(_) | None => return Err(self.refuse_at(Refused::Having, at)),
        }))
    }
}

impl Parser<'_> {
    /// One `GROUP BY` term: a column, or a calendar rounding of one.
    ///
    /// **Parsed with the same code a select-list entry is**, which is the point rather than a
    /// convenience: the lowering has to check that the two agree, and a comparison is only
    /// meaningful between things read the same way. It also means the unit is already checked
    /// here - `parse::scalar` raises `TruncUnit` for a boundary the calendar does not have - so
    /// this only has to decide whether the shape is one there is a plan for.
    pub(super) fn grouping(&mut self) -> Result<Grouping> {
        let at = self.at();
        // `GROUP BY ROLLUP(a, b)` is PostgreSQL's spelling of what this surface writes as
        // `GROUP BY a, b WITH ROLLUP`. Caught here rather than left to `expr`, which would read
        // it as a call to a function nobody defined and say so - a true sentence about the wrong
        // problem, when the statement is one word away from being answered.
        if matches!(self.word(), Some(w) if matches!(w.to_ascii_uppercase().as_str(),
            "ROLLUP" | "CUBE" | "GROUPINGSETS"))
        {
            return Err(self.refuse_at(Refused::GroupExpression, at));
        }
        let parsed = self.expr()?;
        let [leaf] = parsed.leaves.as_slice() else {
            // No column, or two of them: `GROUP BY 1 + 1` groups by nothing, and
            // `GROUP BY a + b` by a value no column holds.
            return Err(self.refuse_at(Refused::GroupExpression, at));
        };
        let Proj::Column(name) = &leaf.proj else {
            // `GROUP BY count(*)`, which is the answer rather than something to group it by.
            return Err(self.refuse_at(Refused::GroupExpression, at));
        };
        let name = name.clone();
        match parsed.expr.is_identity() {
            true => Ok(Grouping { name, bucket: None, at }),
            false => match bucket_of(Some(&parsed.expr)) {
                Some(unit) => Ok(Grouping { name, bucket: Some(unit), at }),
                None => Err(self.refuse_at(Refused::GroupExpression, at)),
            },
        }
    }

    /// `GROUPING SETS ((a, b), (a), ())`: the columns it names, and the sets it names them in.
    ///
    /// Two things come back because the clause says two: the union of every column mentioned,
    /// which is the `GROUP BY` list a plain statement would have written, and which of those
    /// each set holds. The union is built in first-seen order, so a set is a list of positions
    /// into it and `MAX_GROUP_COLUMNS` bounds the widest set by bounding the union.
    ///
    /// A bare term is the one-column set, which is what every dialect that has this reads
    /// `GROUPING SETS (a, (a, b))` as.
    fn grouping_sets(&mut self, group_at: usize) -> Result<(Vec<Grouping>, GroupingSets)> {
        let at = self.at();
        self.expect_word("GROUPING", "GROUPING SETS")?;
        self.expect_word("SETS", "SETS after GROUPING")?;
        self.expect(&Tok::LParen, "( after GROUPING SETS")?;

        let mut by: Vec<Grouping> = Vec::new();
        let mut of: Vec<Vec<usize>> = Vec::new();
        loop {
            let set_at = self.at();
            let mut set: Vec<usize> = Vec::new();
            match self.eat(&Tok::LParen) {
                // `()` is the empty set - the grand total - and is the reason this is a `match`
                // rather than a loop that insists on one term.
                true => {
                    if !self.eat(&Tok::RParen) {
                        loop {
                            set.push(self.intern_grouping(&mut by)?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                        self.expect(&Tok::RParen, ") after a grouping set")?;
                    }
                }
                false => set.push(self.intern_grouping(&mut by)?),
            }
            // `(a, a)` is one column written twice, and the combination of a column with itself
            // is every record paired with itself. The same argument `GROUP BY c, c` gets.
            let mut sorted = set.clone();
            sorted.sort_unstable();
            let deduped = {
                let mut d = sorted.clone();
                d.dedup();
                d
            };
            if deduped.len() != sorted.len() {
                return Err(self.refuse_at(Refused::Shape, set_at));
            }
            // A set written twice is one set, and answering it twice would double every row it
            // holds. Refused rather than folded, so the statement says what it means.
            if of.contains(&deduped) {
                return Err(self.refuse_at(Refused::Shape, set_at));
            }
            of.push(deduped);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen, ") after GROUPING SETS")?;

        if by.len() > MAX_GROUP_COLUMNS {
            return Err(self.refuse_at(Refused::Shape, group_at));
        }
        if of.len() > MAX_GROUPING_SETS {
            return Err(self.refuse_at(Refused::GroupingSets, at));
        }
        sort_sets(&mut of);
        Ok((by, GroupingSets { of, at }))
    }

    /// One term of a grouping set, as a position in the list of columns the clause names.
    ///
    /// Interning rather than pushing, because the sets overlap by design: `((a, b), (a))` names
    /// `a` twice and is one column grouped two ways, not two columns. Two terms naming one
    /// column with *different* boundaries - `date_trunc('day', ts)` in one set and
    /// `date_trunc('month', ts)` in another - are two different groupings of one column and
    /// have no single axis to be, so they are refused rather than silently folded onto the
    /// boundary that happened to be read first.
    fn intern_grouping(&mut self, by: &mut Vec<Grouping>) -> Result<usize> {
        let g = self.grouping()?;
        match by.iter().position(|held| held.name.column == g.name.column) {
            Some(at) if by[at].bucket == g.bucket => Ok(at),
            Some(_) => Err(self.refuse_at(Refused::Shape, g.at)),
            None => {
                by.push(g);
                Ok(by.len() - 1)
            }
        }
    }
}

/// `ROLLUP(a, b, c)`: every prefix, longest first.
///
/// `[[0,1,2], [0,1], [0], []]` - the detail, then each subtotal in decreasing detail, then the
/// grand total. `n + 1` sets for `n` columns.
fn rollup(n: usize) -> Vec<Vec<usize>> {
    (0..=n).rev().map(|len| (0..len).collect()).collect()
}

/// `CUBE(a, b, c)`: every subset. `2^n` sets for `n` columns.
///
/// Ordered by [`sort_sets`] rather than by the bit pattern that generated them, because the order
/// is the answer's row order and a reader should not have to know how the subsets were counted.
fn cube(n: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> =
        (0..1u32 << n).map(|mask| (0..n).filter(|i| mask >> i & 1 == 1).collect()).collect();
    sort_sets(&mut out);
    out
}

/// The order a grouping-sets answer's rows come out in: longest set first, and lexicographic by
/// position within a length.
///
/// **Written down because there is no `ORDER BY` to override it.** An answer whose rows are one
/// grouping per set has no single list to sort - see [`Refused::RollupOrder`] - so the order the
/// branches are built in is the order the client gets, and it has to be a property of the
/// statement rather than of whichever loop happened to produce the sets.
fn sort_sets(of: &mut [Vec<usize>]) {
    of.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
}

/// The column and boundary a `date_trunc` over a bare column names, when that is what it is.
///
/// **One shape-matcher, used from three places**: parsing a `GROUP BY` term, normalising a
/// `SELECT DISTINCT`, and checking in the lowering that the select list and the `GROUP BY` agree.
/// Written once because the third of those is a comparison against the first two, and a second
/// spelling of the pattern would eventually accept something they did not.
///
/// `date_trunc` and ClickHouse's `dateTrunc` both parse to `Func::DateTrunc`, so they compare
/// equal here without either being named - which is the right leniency, for free.
/// The column itself is not in the expression - [`Scalar::Value`] stands in for it - so only the
/// boundary comes back, and the caller takes the column from the item's leaf.
pub(crate) fn bucket_of(apply: Option<&Scalar>) -> Option<big_civil::Unit> {
    let Scalar::Call { func: Func::DateTrunc, args } = apply? else { return None };
    let [Scalar::Literal(Literal::Str(unit)), Scalar::Value] = args.as_slice() else {
        return None;
    };
    big_civil::Unit::parse(unit)
}
