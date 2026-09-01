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
use crate::ast::{Cond, Having, HavingAgg, Join, Order, OrderKey, Proj, Select, Source};
use crate::error::{Refused, Result, SqlError};
use crate::lex::Tok;
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

        // Two columns at most. A third would be a pass over the second column per pair of the
        // first two, which is a cost that grows with the product of three cardinalities.
        let group_at = self.at();
        let group_by = if self.eat_word("GROUP") {
            self.expect_word("BY", "BY after GROUP")?;
            let mut by = vec![self.name("a column to group by")?];
            while self.eat(&Tok::Comma) {
                by.push(self.name("a column to group by")?);
            }
            if by.len() > 2 {
                return Err(self.refuse_at(Refused::Shape, group_at));
            }
            by
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
                        Proj::Column(name) => by.push(name.clone()),
                        // `SELECT DISTINCT count(*)`, `SELECT DISTINCT *`: distinct over
                        // something that is already one value, or over identities that are
                        // already distinct.
                        _ => return Err(self.refuse_at(Refused::Shape, item.at)),
                    }
                }
                if by.len() > 2 {
                    return Err(self.refuse_at(Refused::MultiDistinct, items[2].at));
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
        const KEYWORDS: [&str; 20] = [
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
        for kw in ["LEFT", "RIGHT", "FULL"] {
            if self.word_is(kw) {
                return Err(self.refuse(Refused::OuterJoin));
            }
        }
        for kw in ["CROSS", "NATURAL"] {
            if self.word_is(kw) {
                return Err(self.refuse(Refused::Joins));
            }
        }
        self.eat_word("INNER");
        if !self.eat_word("JOIN") {
            return Ok(None);
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
        Ok(Some(Join { source, left, right, at }))
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

    /// `HAVING <aggregate> <op> <value>`.
    ///
    /// One aggregate, one comparison, one value. **Which** aggregate is legal is not decided
    /// here: a grouped answer carries exactly one number per group, and whether the one named
    /// is the one the select list asked for is a question about the select list. The lowering
    /// asks it. This one only refuses what is not an aggregate at all, so that
    /// `HAVING country = 'GB'` is named as a `HAVING` this surface does not take rather than
    /// as a syntax error somewhere after it.
    pub(super) fn having(&mut self) -> Result<Having> {
        let at = self.at();
        let Ok(name) = self.bare_ident("an aggregate") else {
            return Err(self.refuse_at(Refused::Having, at));
        };
        let agg = match self.aggregate(&name)?.map(|a| (a.proj, a.filter.is_some())) {
            // As in `ORDER BY`: a filtered aggregate is named by its alias, not by repeating
            // the condition that made it.
            Some((_, true)) => return Err(self.refuse_at(Refused::Having, at)),
            Some((Proj::Count, _)) => HavingAgg::Count,
            Some((Proj::Agg { func, field }, _)) => HavingAgg::Agg { func, field },
            Some((Proj::Avg(field), _)) => HavingAgg::Avg(field),
            // `count(DISTINCT x)` in a HAVING is a second grouping inside the first, and a
            // bare column is a predicate on a value no group holds.
            Some(_) | None => return Err(self.refuse_at(Refused::Having, at)),
        };

        let Some(Tok::Op(op)) = self.peek() else {
            return Err(self.syntax("a comparison after the aggregate"));
        };
        let op = *op;
        self.i += 1;

        // Any literal, not only a whole number: a threshold on a decimal field is written the
        // way the field is written, and `Shape::resolve` converts it with the same code a
        // `WHERE` comparison goes through.
        let value = self.literal("a value to compare the aggregate against")?;

        Ok(Having { agg, op, value, at })
    }
}
