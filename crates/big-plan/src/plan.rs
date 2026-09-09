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

//! Resolving a parsed query against a schema.
//!
//! Everything the executor could get wrong at run time is decided here instead: the field
//! exists, the operator means something for its class, the argument really is a bitmap and not
//! a count. What comes out cannot fail for those reasons, which is why [`Plan`] has no
//! variants for "maybe a field".

use crate::ast::{Call, Expr, Literal};
use crate::error::{PlanError, Result};
use crate::schema::{FieldClass, Keyed, Schema, TimeUnit};

/// Comparisons an integer field accepts. Deliberately the same set the storage layer already
/// implements, so planning never promises something the executor has to emulate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

/// A resolved query producing a set of records.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Rows {
    Compare {
        field: String,
        op: CmpOp,
        value: u64,
    },
    /// The same, against a signed field. The bound stays an `i64` all the way down; the bias
    /// is applied in the storage layer, which is the only place that knows the field's depth.
    ///
    /// A date comparison is one of these. The stored value is a count from the Unix epoch, so
    /// once `'2024-01-15'` has become a number there is nothing left for a temporal plan to say.
    CompareSigned {
        field: String,
        op: CmpOp,
        value: i64,
    },
    /// The same, against a float field.
    ///
    /// **The threshold travels as `f64::to_bits`, not as an `f64`.** Two reasons, and the second
    /// is the load-bearing one: a plan is compared for equality all over the test corpus, and
    /// `PartialEq` on a float is not reflexive, so an `f64` here would cost this enum its `Eq`
    /// and cost it to everything holding one. The bits are the same number - `from_bits` is
    /// exact and total - and the storage layer reads them back before it does anything with
    /// them.
    ///
    /// Unrounded, because rounding it needs the field's width and the operator together; see
    /// `big_db::float::encode_bound`.
    CompareFloat {
        field: String,
        op: CmpOp,
        /// `f64::to_bits` of the written threshold.
        bits: u64,
    },
    Key {
        field: String,
        value: String,
    },
    /// Every key of a field matching a `LIKE` pattern, unioned.
    ///
    /// **A set operation, not a scan.** A keyed field interns each distinct string once, so
    /// this walks that dictionary and unions the bitmaps of the keys that match - the field's
    /// cardinality, paid once, where a row engine pays the record count. Which is why it is a
    /// `Rows` variant rather than a filter applied to something else's answer: it *is* a
    /// bitmap, and everything that composes bitmaps composes it.
    ///
    /// Only a keyed field has a dictionary to walk. `big_db` refuses the rest by name rather
    /// than answering an empty set, because an integer holds no strings and "nothing matched"
    /// would be the wrong answer wearing a right one's clothes.
    KeyLike {
        field: String,
        /// SQL's two wildcards: `%` for any run, `_` for one, `\` to escape either.
        pattern: String,
        /// `ILIKE`: fold both sides before comparing. The only difference between the two.
        fold: bool,
    },
    /// A key restricted to a window of time, answered from the views a time quantum field
    /// writes rather than by filtering everything it ever recorded.
    ///
    /// An absent bound is genuinely absent, not a very large number. Standing in a sentinel
    /// would push a date conversion out to the limits of the calendar for no reason.
    KeyBetween {
        field: String,
        value: String,
        from: Option<i64>,
        to: Option<i64>,
    },
    Bool {
        field: String,
        value: bool,
    },
    Intersect(Vec<Rows>),
    Union(Vec<Rows>),
    Difference(Box<Rows>, Box<Rows>),
    /// Complement against every record that exists, which is why the table is carried along.
    Not(Box<Rows>),
    All,
    /// One record in every `stride`, by record id.
    ///
    /// **Sampled on the *low bits of the id*, not on blocks of them**, and the difference is the
    /// whole reason this is answerable at all. A roaring container holds 65,536 consecutive
    /// ids, so "every eighth id" is a fixed bit pattern inside each container - one word
    /// constant, ANDed - which costs a pass over containers and never decodes a record. Taking
    /// every eighth *container* would be as cheap and would be a different thing: ids are
    /// handed out in write order, so a block of them is a window of time, and a number drawn
    /// from one would lean whichever way the data drifted.
    ///
    /// `stride` is at least two. It need not divide a container: the pattern repeats every
    /// `stride` ids whatever it is, and a container that starts part-way through one begins at
    /// a known phase - so the mask is still built rather than walked.
    Sample {
        inner: Box<Rows>,
        stride: u32,
    },
}

/// One column a grouping is over, and how its values become sets of records.
///
/// **The two ways a column can have groups at all**, which is why they are one type rather than
/// two plans: a keyed column has a dictionary to walk, and a bit-sliced temporal one has an
/// ordering to cut into calendar buckets. Mixing them in one grouping - `GROUP BY country,
/// date_trunc('month', ts)` - then costs nothing extra, because the walk asks each level only
/// for "the sets your values make".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Level {
    /// A keyed column: one set per row of its dictionary.
    Keyed { field: String },
    /// A bit-sliced temporal column: one set per calendar bucket. See [`Plan::GroupByBucket`].
    Bucket { field: String, unit: big_civil::Unit, max_buckets: usize },
}

impl Level {
    /// The column this level groups by.
    pub fn field(&self) -> &str {
        match self {
            Self::Keyed { field } | Self::Bucket { field, .. } => field,
        }
    }
}

/// A resolved query, and what kind of answer it produces.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Plan {
    Rows {
        table: String,
        rows: Rows,
    },
    Count {
        table: String,
        rows: Rows,
    },
    Sum {
        table: String,
        rows: Rows,
        field: String,
    },
    Min {
        table: String,
        rows: Rows,
        field: String,
    },
    Max {
        table: String,
        rows: Rows,
        field: String,
    },
    /// Every row of a keyed field present in `rows`, with a count each.
    Distinct {
        table: String,
        rows: Rows,
        field: String,
    },
    /// The same, ordered by count and cut to `n`.
    TopN {
        table: String,
        rows: Rows,
        field: String,
        n: usize,
    },
    /// The same, but each group carries an aggregate of its own records instead of a count.
    GroupBy {
        table: String,
        rows: Rows,
        field: String,
        aggregate: Box<Plan>,
    },
    /// One group per calendar bucket of a bit-sliced temporal column that holds a record.
    ///
    /// **The one grouping over a column with no rows to enumerate.** A keyed field interns each
    /// value and keeps a bitmap per row, so grouping it is a walk of that dictionary. A `DATE`
    /// keeps bit planes and nothing else - there is no dictionary, and `Distinct` over one is
    /// refused for exactly that reason.
    ///
    /// What makes this answerable anyway is that a bucket is a **range**: the records in one are
    /// `x >= start AND x < next`, which is the read the bit planes already answer. So the groups
    /// come from the calendar rather than from the data, and the cost is a range scan per bucket
    /// instead of a dictionary walk.
    ///
    /// A record holding no value in the column is in **no** bucket, which is what `GROUP BY`
    /// means and is why these counts sum to the number of records that hold a value rather than
    /// to `count(*)`.
    GroupByBucket {
        table: String,
        rows: Rows,
        /// The temporal column, which must be bit-sliced: a `DATE` or a `DATETIME`.
        field: String,
        /// The calendar boundary the buckets fall on.
        unit: big_civil::Unit,
        /// How many buckets the column's values may span. Never absent, and part of the plan for
        /// the reason [`Plan::GroupByTuple`]'s `max_passes` is: the number of them is what this
        /// costs, and a cut applied to the answer would be a cut applied after paying for it.
        ///
        /// Counted per node. Each walks its own values' range, so a coordinator can hold up to
        /// one bound's worth per owner - the same semantics `max_passes` has.
        max_buckets: usize,
        /// What each bucket carries, planned against a placeholder bitmap exactly as a
        /// [`Plan::GroupBy`]'s aggregate is.
        aggregate: Box<Plan>,
    },
    /// One group per combination of values two or more columns hold records for.
    ///
    /// **Not a composite key, which this index never stored.** For each value of the first column
    /// the records holding it are a set; grouping *those* by the second is an ordinary grouping
    /// over a narrower set, and grouping those by the third is one again. So an N-column grouping
    /// is a walk of a tree, and what it costs is the number of *passes* - one per node of every
    /// level but the last.
    ///
    /// That number is what [`Self::GroupByTuple::max_passes`] bounds, and why the bound is in the
    /// plan rather than applied to the answer. Checking it at the top of each level bounds the
    /// product as well as the first column: the frontier after level one *is* the pass count for
    /// level two.
    GroupByTuple {
        table: String,
        rows: Rows,
        /// The columns, outermost first. Never fewer than two - one is a [`Plan::GroupBy`].
        levels: Vec<Level>,
        /// What each combination carries, planned against a placeholder bitmap exactly as a
        /// [`Plan::GroupBy`]'s aggregate is.
        aggregate: Box<Plan>,
        /// How many passes over an inner column this may make. Never absent.
        max_passes: usize,
    },
    /// The stored values of some columns, for the first `limit` matching records - or for every
    /// one of them where there is no limit.
    ///
    /// **The one plan that reads values back rather than counting bits**, and the reason its
    /// limit is part of the plan rather than a view of the answer. A record's value in a
    /// bit-sliced field is not stored anywhere as a number: reconstructing it means reading one
    /// bit per plane, so a projection costs a point read per record per column. A cut applied
    /// after the reads would save nothing, so the number travels with the plan and bounds the
    /// loop that does them.
    ///
    /// Absent is a full scan, and it is the caller's to ask for: a statement with no `LIMIT`
    /// reads every matching record, at that cost, however many there are.
    ///
    /// Only bit-sliced columns. A keyed column has no read back from a record to its string at
    /// all, and the planner refuses one by name.
    Project {
        table: String,
        rows: Rows,
        /// The columns, in the order the answer's cells go in.
        fields: Vec<String>,
        /// How many records to read, or [`None`] to read every match.
        limit: Option<usize>,
    },
}

impl Plan {
    pub fn table(&self) -> &str {
        match self {
            Self::Rows { table, .. }
            | Self::Count { table, .. }
            | Self::Sum { table, .. }
            | Self::Min { table, .. }
            | Self::Max { table, .. }
            | Self::Distinct { table, .. }
            | Self::TopN { table, .. }
            | Self::GroupBy { table, .. }
            | Self::GroupByBucket { table, .. }
            | Self::GroupByTuple { table, .. }
            | Self::Project { table, .. } => table,
        }
    }
}

/// Resolves a parsed call against `schema`, scoped to one table.
///
/// The table is a parameter rather than part of the query text, matching the shape of the
/// request that carries it: a query is always asked *of* an index.
pub fn plan(table: &str, call: &Call, schema: &impl Schema) -> Result<Plan> {
    if !schema.has_table(table) {
        return Err(PlanError::UnknownTable(table.to_string()));
    }
    let ctx = Ctx { table, schema };

    match call.name.as_str() {
        "Count" => {
            let inner = ctx.one_rows_arg("Count", &call.args)?;
            Ok(Plan::Count { table: table.to_string(), rows: inner })
        }
        "Sum" | "Min" | "Max" => ctx.aggregate(call),
        "Distinct" | "TopN" => ctx.grouped(call),
        "GroupBy" => ctx.group_by(call),
        "GroupByBucket" => ctx.group_by_bucket(call),
        "GroupByTuple" => ctx.group_by_tuple(call),
        "Project" => ctx.project(call),
        _ => Ok(Plan::Rows { table: table.to_string(), rows: ctx.rows(call)? }),
    }
}

struct Ctx<'a, S: Schema> {
    table: &'a str,
    schema: &'a S,
}

impl<S: Schema> Ctx<'_, S> {
    fn class(&self, field: &str) -> Result<FieldClass> {
        self.schema.field_class(self.table, field).ok_or_else(|| PlanError::UnknownField {
            table: self.table.to_string(),
            field: field.to_string(),
        })
    }

    fn one_rows_arg(&self, call: &'static str, args: &[Expr]) -> Result<Rows> {
        match args {
            [Expr::Call(inner)] => self.rows(inner),
            [_] => Err(PlanError::BadArgument { call, want: "a bitmap call" }),
            _ => Err(PlanError::Arity { call, want: "one bitmap", got: args.len() }),
        }
    }

    /// `Sum(<bitmap>, field=<name>)`, and the same shape for `Min` and `Max`.
    ///
    /// The field is named rather than positional because the two arguments are not
    /// interchangeable and a reader should not have to remember which order they go in.
    fn aggregate(&self, call: &Call) -> Result<Plan> {
        // Leaked into every error message below, so it has to be the name the user wrote.
        let name: &'static str = match call.name.as_str() {
            "Min" => "Min",
            "Max" => "Max",
            _ => "Sum",
        };
        let (rows, field) = match call.args.as_slice() {
            [Expr::Call(inner), Expr::Named { name: arg, value }] if arg == "field" => {
                (self.rows(inner)?, str_arg(name, value)?)
            }
            [Expr::Call(inner), Expr::Compare { field, op, value }]
                if field == "field" && op == "=" =>
            {
                let Literal::Str(f) = value else {
                    return Err(PlanError::BadArgument { call: name, want: "field=<name>" });
                };
                (self.rows(inner)?, f.clone())
            }
            [_, _] => {
                return Err(PlanError::BadArgument {
                    call: name,
                    want: "a bitmap and field=<name>",
                })
            }
            other => {
                return Err(PlanError::Arity {
                    call: name,
                    want: "a bitmap and field=<name>",
                    got: other.len(),
                })
            }
        };

        // Which class it is decides whether the answer comes back signed or folded from values,
        // and both are settled in the executor against the field rather than here against the
        // call - a `Sum` is a `Sum` either way.
        let class = self.class(&field)?;
        if !aggregable(class, name) {
            return Err(PlanError::OperatorNotAllowed {
                field,
                op: name.to_string(),
                class: class_name(class),
            });
        }
        let table = self.table.to_string();
        Ok(match name {
            "Min" => Plan::Min { table, rows, field },
            "Max" => Plan::Max { table, rows, field },
            _ => Plan::Sum { table, rows, field },
        })
    }

    /// `Distinct(<bitmap>, field=<name>)` and `TopN(<bitmap>, field=<name>, n=<count>)`.
    fn grouped(&self, call: &Call) -> Result<Plan> {
        let top = call.name == "TopN";
        let name: &'static str = if top { "TopN" } else { "Distinct" };

        let mut rows = None;
        let mut field = None;
        let mut n = None;
        for arg in &call.args {
            match arg {
                Expr::Call(c) if rows.is_none() => rows = Some(self.rows(c)?),
                Expr::Named { name: a, value } if a == "field" => {
                    field = Some(str_arg(name, value)?)
                }
                Expr::Named { name: a, value } if a == "n" && top => match value.as_ref() {
                    Expr::Literal(Literal::Int(v)) => n = Some(*v as usize),
                    _ => return Err(PlanError::BadArgument { call: name, want: "n=<count>" }),
                },
                _ => {
                    return Err(PlanError::BadArgument {
                        call: name,
                        want: "a bitmap and field=<name>",
                    })
                }
            }
        }

        let (Some(rows), Some(field)) = (rows, field) else {
            return Err(PlanError::Arity {
                call: name,
                want: "a bitmap and field=<name>",
                got: call.args.len(),
            });
        };
        // Grouping counts rows, and only a keyed field has rows to count.
        self.expect_keyed(name, &field)?;

        let table = self.table.to_string();
        Ok(if top {
            Plan::TopN { table, rows, field, n: n.unwrap_or(usize::MAX) }
        } else {
            Plan::Distinct { table, rows, field }
        })
    }

    /// `GroupBy(<bitmap>, field=<name>, aggregate=<call>)`.
    ///
    /// The aggregate is planned against a placeholder bitmap, because what it will actually
    /// run over is one group at a time and no group is known yet. Only its shape and its own
    /// field are being checked here.
    fn group_by(&self, call: &Call) -> Result<Plan> {
        let mut rows = None;
        let mut field = None;
        let mut aggregate = None;
        for arg in &call.args {
            match arg {
                Expr::Call(c) if rows.is_none() => rows = Some(self.rows(c)?),
                Expr::Named { name: a, value } if a == "field" => {
                    field = Some(str_arg("GroupBy", value)?)
                }
                Expr::Named { name: a, value } if a == "aggregate" => match value.as_ref() {
                    Expr::Call(c) => aggregate = Some(self.bare_aggregate(c)?),
                    _ => {
                        return Err(PlanError::BadArgument {
                            call: "GroupBy",
                            want: "aggregate=<Sum|Min|Max>(field=...)",
                        })
                    }
                },
                _ => {
                    return Err(PlanError::BadArgument {
                        call: "GroupBy",
                        want: "a bitmap, field=<name>, and optionally aggregate=<call>",
                    })
                }
            }
        }

        let (Some(rows), Some(field)) = (rows, field) else {
            return Err(PlanError::Arity {
                call: "GroupBy",
                want: "a bitmap and field=<name>",
                got: call.args.len(),
            });
        };
        self.expect_keyed("GroupBy", &field)?;

        let table = self.table.to_string();
        let aggregate = aggregate.unwrap_or(Plan::Count { table: table.clone(), rows: Rows::All });
        Ok(Plan::GroupBy { table, rows, field, aggregate: Box::new(aggregate) })
    }

    /// `GroupByBucket(<bitmap>, field=<name>, unit='month', n=<count>, [aggregate=<call>])`.
    ///
    /// `unit=` takes `date_trunc`'s own vocabulary, so the two cannot come to disagree about what
    /// a month is; `n=` bounds the work the way [`Self::group_by_pair`]'s does.
    fn group_by_bucket(&self, call: &Call) -> Result<Plan> {
        const WANT: &str = "a bitmap, field=<name>, unit=<year|month|day|...> and n=<count>";
        let (mut rows, mut field, mut unit, mut n, mut aggregate) = (None, None, None, None, None);
        for arg in &call.args {
            match arg {
                Expr::Call(c) if rows.is_none() => rows = Some(self.rows(c)?),
                Expr::Named { name, value } if name == "field" => {
                    field = Some(str_arg("GroupByBucket", value)?)
                }
                Expr::Named { name, value } if name == "unit" => match value.as_ref() {
                    Expr::Literal(Literal::Str(u)) => {
                        unit = Some(big_civil::Unit::parse(u).ok_or(PlanError::BadRounding {
                            call: format!("date_trunc('{u}', ...)"),
                            why: "a boundary the calendar has: year, quarter, month, week, day, \
                                  hour, minute or second",
                        })?)
                    }
                    _ => {
                        return Err(PlanError::BadArgument {
                            call: "GroupByBucket",
                            want: "unit=<year|month|day|...>",
                        })
                    }
                },
                Expr::Named { name, value } if name == "n" => match value.as_ref() {
                    Expr::Literal(Literal::Int(v)) => n = Some(*v as usize),
                    _ => {
                        return Err(PlanError::BadArgument {
                            call: "GroupByBucket",
                            want: "n=<count>",
                        })
                    }
                },
                Expr::Named { name, value } if name == "aggregate" => match value.as_ref() {
                    Expr::Call(c) => aggregate = Some(self.bare_aggregate(c)?),
                    _ => {
                        return Err(PlanError::BadArgument {
                            call: "GroupByBucket",
                            want: "aggregate=<Sum|Min|Max>(field=...)",
                        })
                    }
                },
                _ => return Err(PlanError::BadArgument { call: "GroupByBucket", want: WANT }),
            }
        }

        let (Some(rows), Some(field), Some(unit), Some(max_buckets)) = (rows, field, unit, n)
        else {
            return Err(PlanError::Arity {
                call: "GroupByBucket",
                want: WANT,
                got: call.args.len(),
            });
        };
        // The mirror image of `expect_keyed`: this grouping wants a column with no dictionary
        // and an ordering instead, which is exactly what a keyed field does not have.
        check_boundary(&field, self.class(&field)?, unit)?;

        let table = self.table.to_string();
        let aggregate = aggregate.unwrap_or(Plan::Count { table: table.clone(), rows: Rows::All });
        Ok(Plan::GroupByBucket {
            table,
            rows,
            field,
            unit,
            max_buckets,
            aggregate: Box::new(aggregate),
        })
    }

    /// `GroupByTuple(<bitmap>, by=<name>, by=Bucket(field=<name>, unit=..., n=...), ...,
    /// n=<passes>, [aggregate=<call>])`.
    ///
    /// A **repeating** `by=`, the way `Project` repeats `field=`, because the levels are an
    /// ordered list rather than a fixed pair - and because a level is either a bare name or a
    /// `Bucket(...)`, which the argument loop already tells apart by whether it is a call.
    fn group_by_tuple(&self, call: &Call) -> Result<Plan> {
        const WANT: &str = "a bitmap, two or more by=<name> or by=Bucket(...), and n=<passes>";
        let (mut rows, mut levels, mut n, mut aggregate) = (None, Vec::new(), None, None);
        for arg in &call.args {
            match arg {
                Expr::Call(c) if rows.is_none() => rows = Some(self.rows(c)?),
                Expr::Named { name, value } if name == "by" => levels.push(self.level(value)?),
                Expr::Named { name, value } if name == "n" => match value.as_ref() {
                    Expr::Literal(Literal::Int(v)) => n = Some(*v as usize),
                    _ => {
                        return Err(PlanError::BadArgument {
                            call: "GroupByTuple",
                            want: "n=<passes>",
                        })
                    }
                },
                Expr::Named { name, value } if name == "aggregate" => match value.as_ref() {
                    Expr::Call(c) => aggregate = Some(self.bare_aggregate(c)?),
                    _ => {
                        return Err(PlanError::BadArgument {
                            call: "GroupByTuple",
                            want: "aggregate=<Sum|Min|Max>(field=...)",
                        })
                    }
                },
                _ => return Err(PlanError::BadArgument { call: "GroupByTuple", want: WANT }),
            }
        }

        let (Some(rows), Some(max_passes)) = (rows, n) else {
            return Err(PlanError::Arity {
                call: "GroupByTuple",
                want: WANT,
                got: call.args.len(),
            });
        };
        // One level is a `GroupBy` and none is a `Count`; naming this a tuple would be a second
        // spelling of a plan that already exists.
        if levels.len() < 2 {
            return Err(PlanError::Arity {
                call: "GroupByTuple",
                want: WANT,
                got: call.args.len(),
            });
        }

        let table = self.table.to_string();
        let aggregate = aggregate.unwrap_or(Plan::Count { table: table.clone(), rows: Rows::All });
        Ok(Plan::GroupByTuple { table, rows, levels, aggregate: Box::new(aggregate), max_passes })
    }

    /// One `by=` argument: a bare column name, or `Bucket(field=..., unit=..., n=...)`.
    fn level(&self, value: &Expr) -> Result<Level> {
        match value {
            Expr::Ident(field) | Expr::Literal(Literal::Str(field)) => {
                self.expect_keyed("GroupByTuple", field)?;
                Ok(Level::Keyed { field: field.clone() })
            }
            Expr::Call(c) if c.name == "Bucket" => {
                let (mut field, mut unit, mut n) = (None, None, None);
                for arg in &c.args {
                    match arg {
                        Expr::Named { name, value } if name == "field" => {
                            field = Some(str_arg("Bucket", value)?)
                        }
                        Expr::Named { name, value } if name == "unit" => match value.as_ref() {
                            Expr::Literal(Literal::Str(u)) => {
                                unit = Some(big_civil::Unit::parse(u).ok_or(
                                    PlanError::BadRounding {
                                        call: format!("date_trunc('{u}', ...)"),
                                        why: "a boundary the calendar has",
                                    },
                                )?)
                            }
                            _ => {
                                return Err(PlanError::BadArgument {
                                    call: "Bucket",
                                    want: "unit=<year|month|day|...>",
                                })
                            }
                        },
                        Expr::Named { name, value } if name == "n" => match value.as_ref() {
                            Expr::Literal(Literal::Int(v)) => n = Some(*v as usize),
                            _ => {
                                return Err(PlanError::BadArgument {
                                    call: "Bucket",
                                    want: "n=<count>",
                                })
                            }
                        },
                        _ => {
                            return Err(PlanError::BadArgument {
                                call: "Bucket",
                                want: "field=<name>, unit=<boundary> and n=<count>",
                            })
                        }
                    }
                }
                let (Some(field), Some(unit), Some(max_buckets)) = (field, unit, n) else {
                    return Err(PlanError::Arity {
                        call: "Bucket",
                        want: "field=<name>, unit=<boundary> and n=<count>",
                        got: c.args.len(),
                    });
                };
                check_boundary(&field, self.class(&field)?, unit)?;
                Ok(Level::Bucket { field, unit, max_buckets })
            }
            _ => Err(PlanError::BadArgument {
                call: "GroupByTuple",
                want: "by=<name> or by=Bucket(field=..., unit=..., n=...)",
            }),
        }
    }

    /// `Project(<bitmap>, field=<name>, field=<name>, ..., [n=<count>])`.
    ///
    /// The one call whose field argument may repeat, because a projection is a list of columns
    /// and their order is the order the answer's cells go in. `n` is optional, and leaving it
    /// out asks for every matching record: see [`Plan::Project`] for what that costs.
    fn project(&self, call: &Call) -> Result<Plan> {
        let mut rows = None;
        let mut fields: Vec<String> = Vec::new();
        let mut n = None;
        for arg in &call.args {
            match arg {
                Expr::Call(c) if rows.is_none() => rows = Some(self.rows(c)?),
                Expr::Named { name, value } if name == "field" => {
                    fields.push(str_arg("Project", value)?)
                }
                Expr::Named { name, value } if name == "n" => match value.as_ref() {
                    Expr::Literal(Literal::Int(v)) => n = Some(*v as usize),
                    _ => return Err(PlanError::BadArgument { call: "Project", want: "n=<count>" }),
                },
                _ => {
                    return Err(PlanError::BadArgument {
                        call: "Project",
                        want: "a bitmap, one or more field=<name>, and an optional n=<count>",
                    })
                }
            }
        }

        let Some(rows) = rows else {
            return Err(PlanError::Arity {
                call: "Project",
                want: "a bitmap, one or more field=<name>, and an optional n=<count>",
                got: call.args.len(),
            });
        };
        let limit = n;

        // No `field=` at all is `SELECT *`: every column the table declares, in declaration
        // order. Expanded here rather than by the caller because only a schema knows the list -
        // and through [`crate::expanded_columns`] rather than by hand, because the shape that
        // names these columns expands the same `*` and the two have to come out the same.
        if fields.is_empty() {
            let fields = crate::expanded_columns(self.schema, self.table);
            // Nothing a projection could read - a table with no fields, or one whose engine
            // keeps no values. `SELECT *` falls back to record ids, which is the answer it gave
            // before it expanded to anything.
            if fields.is_empty() {
                return Ok(Plan::Rows { table: self.table.to_string(), rows });
            }
            return Ok(Plan::Project { table: self.table.to_string(), rows, fields, limit });
        }

        let stores_values = self.schema.stores_values(self.table);
        for field in &fields {
            match self.class(field)? {
                // Every bit-sliced class reconstructs from its planes. Which one it is decides
                // what the stored number has to be turned back into - a sign undone, a float
                // decoded, a count from the epoch read as a date - and all of that is settled
                // below this line, against the field, as it is for every aggregate.
                FieldClass::Integer { .. }
                | FieldClass::Signed
                | FieldClass::Float { .. }
                | FieldClass::Temporal { .. } => {}
                // A keyed or boolean column can be read back only where the values are stored.
                // An index records which records hold a key and never which key a record
                // holds, so on a table without segments there is no read to allow.
                FieldClass::Keyed(_) | FieldClass::Boolean if stores_values => {}
                other => {
                    return Err(PlanError::OperatorNotAllowed {
                        field: field.clone(),
                        // Reads as "`a projection` is not allowed on `country`, which is a
                        // keyed field", which is the sentence somebody who wrote
                        // `SELECT country` needs.
                        op: "a projection".to_string(),
                        class: class_name(other),
                    });
                }
            }
        }

        Ok(Plan::Project { table: self.table.to_string(), rows, fields, limit })
    }

    /// An aggregate written without a bitmap of its own, which is how it appears inside a
    /// `GroupBy`: each group is the bitmap, so writing one would be writing the wrong one.
    ///
    /// `Rows::All` stands in its place and is never evaluated - the executor swaps the group's
    /// records in. Keeping the same `Plan` variants means one place decides what `Sum` means.
    fn bare_aggregate(&self, call: &Call) -> Result<Plan> {
        let table = self.table.to_string();
        if call.name == "Count" {
            return match call.args.len() {
                0 => Ok(Plan::Count { table, rows: Rows::All }),
                n => Err(PlanError::Arity {
                    call: "Count",
                    want: "no arguments inside GroupBy",
                    got: n,
                }),
            };
        }

        let name: &'static str = match call.name.as_str() {
            "Sum" => "Sum",
            "Min" => "Min",
            "Max" => "Max",
            other => return Err(PlanError::UnknownCall(other.to_string())),
        };
        let field = match call.args.as_slice() {
            [Expr::Named { name: a, value }] if a == "field" => str_arg(name, value)?,
            other => {
                return Err(PlanError::Arity { call: name, want: "field=<name>", got: other.len() })
            }
        };
        let class = self.class(&field)?;
        if !aggregable(class, name) {
            return Err(PlanError::OperatorNotAllowed {
                field,
                op: name.to_string(),
                class: class_name(class),
            });
        }
        Ok(match name {
            "Min" => Plan::Min { table, rows: Rows::All, field },
            "Max" => Plan::Max { table, rows: Rows::All, field },
            _ => Plan::Sum { table, rows: Rows::All, field },
        })
    }

    fn expect_keyed(&self, call: &'static str, field: &str) -> Result<()> {
        match self.class(field)? {
            FieldClass::Keyed(_) => Ok(()),
            other => Err(PlanError::OperatorNotAllowed {
                field: field.to_string(),
                op: call.to_string(),
                class: class_name(other),
            }),
        }
    }

    fn rows(&self, call: &Call) -> Result<Rows> {
        match call.name.as_str() {
            "Row" => self.row(call),
            "Rounded" => self.rounded(call),
            "All" => match call.args.len() {
                0 => Ok(Rows::All),
                n => Err(PlanError::Arity { call: "All", want: "no arguments", got: n }),
            },
            "Like" => self.like(call, false),
            "ILike" => self.like(call, true),
            "Not" => Ok(Rows::Not(Box::new(self.one_rows_arg("Not", &call.args)?))),
            "Sample" => self.sample(call),
            "Intersect" => Ok(Rows::Intersect(self.rows_args("Intersect", &call.args)?)),
            "Union" => Ok(Rows::Union(self.rows_args("Union", &call.args)?)),
            "Difference" => match call.args.as_slice() {
                [Expr::Call(a), Expr::Call(b)] => {
                    Ok(Rows::Difference(Box::new(self.rows(a)?), Box::new(self.rows(b)?)))
                }
                other => Err(PlanError::Arity {
                    call: "Difference",
                    want: "exactly two bitmaps",
                    got: other.len(),
                }),
            },
            other => Err(PlanError::UnknownCall(other.to_string())),
        }
    }

    fn rows_args(&self, call: &'static str, args: &[Expr]) -> Result<Vec<Rows>> {
        if args.is_empty() {
            return Err(PlanError::Arity { call, want: "at least one bitmap", got: 0 });
        }
        args.iter()
            .map(|a| match a {
                Expr::Call(c) => self.rows(c),
                _ => Err(PlanError::BadArgument { call, want: "bitmap calls" }),
            })
            .collect()
    }

    /// `Row(field="x", from=<unix seconds>, to=<unix seconds>)`.
    fn row_between(&self, args: &[Expr]) -> Result<Option<Rows>> {
        let (mut field, mut value, mut from, mut to) = (None, None, None, None);
        for arg in args {
            match arg {
                Expr::Named { name, value: v } => match (name.as_str(), v.as_ref()) {
                    ("from", Expr::Literal(Literal::Int(n))) => from = Some(*n as i64),
                    ("to", Expr::Literal(Literal::Int(n))) => to = Some(*n as i64),
                    (f, Expr::Literal(Literal::Str(s))) => {
                        field = Some(f.to_string());
                        value = Some(s.clone());
                    }
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            }
        }

        let (Some(field), Some(value)) = (field, value) else { return Ok(None) };
        if from.is_none() && to.is_none() {
            return Ok(None);
        }
        // **A window reads the views a time quantum field writes.** Against any other keyed
        // field there are none, and the read would answer with an empty set rather than an
        // error - a window that matched nothing, which is what a window that cannot be answered
        // looks like from the outside. Refused here, where the difference is visible.
        match self.class(&field)? {
            FieldClass::Keyed(k) if k.has_time_views() => {}
            other => {
                return Err(PlanError::OperatorNotAllowed {
                    field,
                    op: "a time window".to_string(),
                    class: class_name(other),
                })
            }
        }

        // An open end is the whole of time on that side, not an error: asking for everything
        // since a date is the common case.
        Ok(Some(Rows::KeyBetween { field, value, from, to }))
    }

    /// `Row(field > 5)`, `Row(country="GB")`, `Row(active=true)`.
    ///
    /// This is where the parser's refusal to guess about `=` gets paid off: the schema says
    /// whether `country` is a key or an integer, and the same syntax resolves either way.
    /// `Like(field='pattern')`, and `ILike` for the folded one.
    ///
    /// Written like a `Row` because it is one shape of the same question - which key does a
    /// record hold - and reusing the spelling means somebody who can write `Row(country='GB')`
    /// can write this without learning a second form.
    ///
    /// The field's class is checked here rather than left to the storage layer, so that a
    /// pattern over an integer is refused where every other type mistake in this language is:
    /// at plan time, with the sentence naming what the column actually holds.
    fn like(&self, call: &Call, fold: bool) -> Result<Rows> {
        let called = if fold { "ILike" } else { "Like" };
        let (field, pattern) = match call.args.as_slice() {
            [Expr::Named { name, value }] => match value.as_ref() {
                Expr::Literal(Literal::Str(p)) => (name.clone(), p.clone()),
                _ => return Err(PlanError::BadArgument { call: called, want: "a quoted pattern" }),
            },
            [Expr::Compare { field, op, value }] if op == "=" || op == "==" => match value {
                Literal::Str(p) => (field.clone(), p.clone()),
                _ => return Err(PlanError::BadArgument { call: called, want: "a quoted pattern" }),
            },
            other => {
                return Err(PlanError::Arity {
                    call: called,
                    want: "one field and a quoted pattern",
                    got: other.len(),
                })
            }
        };

        let class = self.class(&field)?;
        match class {
            FieldClass::Keyed(_) => Ok(Rows::KeyLike { field, pattern, fold }),
            // The same refusal an operator mistake gets, and for the same reason: a pattern is
            // an operator on strings, and this column holds none.
            class => Err(PlanError::OperatorNotAllowed {
                field,
                op: called.to_string(),
                class: class_name(class),
            }),
        }
    }

    /// `Rounded(<field> <op> <value>, by='round', digits=<n>)`.
    ///
    /// The one comparison whose bound this layer computes rather than converts, because the bound
    /// depends on the field's scale. See [`mod@crate::rounded`].
    fn rounded(&self, call: &Call) -> Result<Rows> {
        const WANT: &str = "a comparison, by=<round|floor|ceil>, and digits=<n> for a round";
        let (mut compare, mut by, mut digits) = (None, None, None);
        for arg in &call.args {
            match arg {
                Expr::Compare { field, op, value } => compare = Some((field, op, value)),
                Expr::Named { name, value } if name == "by" => match value.as_ref() {
                    Expr::Literal(Literal::Str(s)) => by = Some(s.as_str()),
                    _ => return Err(PlanError::BadArgument { call: "Rounded", want: WANT }),
                },
                Expr::Named { name, value } if name == "digits" => match value.as_ref() {
                    Expr::Literal(Literal::Int(d)) if *d <= u64::from(u8::MAX) => {
                        digits = Some(*d as u8)
                    }
                    _ => return Err(PlanError::BadArgument { call: "Rounded", want: WANT }),
                },
                _ => return Err(PlanError::BadArgument { call: "Rounded", want: WANT }),
            }
        }
        let (Some((field, op, value)), Some(by)) = (compare, by) else {
            return Err(PlanError::Arity { call: "Rounded", want: WANT, got: call.args.len() });
        };
        let by = crate::rounded::By::parse(by, digits)
            .ok_or(PlanError::BadArgument { call: "Rounded", want: WANT })?;
        crate::rounded::rows(field, self.class(field)?, by, op, value)
    }

    /// `Sample(<bitmap>, stride=<n>)`: one record in every `n`, by record id.
    ///
    /// `stride` is named rather than positional for the reason a field is on an aggregate: the
    /// two arguments are not interchangeable and a reader should not have to remember the order.
    ///
    /// **Any stride from two upwards**, because the mask does not have to divide a container to
    /// be a mask: the pattern repeats every `stride` ids, and a container beginning part-way
    /// through one begins at a phase that is arithmetic rather than a search. Rounding a stride
    /// to a nearby power of two would be a different sample wearing the right name.
    fn sample(&self, call: &Call) -> Result<Rows> {
        const WANT: &str = "a bitmap and stride=<2 or more>";
        let (mut inner, mut stride) = (None, None);
        for arg in &call.args {
            match arg {
                Expr::Call(c) => inner = Some(self.rows(c)?),
                Expr::Named { name, value } if name == "stride" => match value.as_ref() {
                    Expr::Literal(Literal::Int(n)) if (2..=u64::from(u32::MAX)).contains(n) => {
                        stride = Some(*n as u32)
                    }
                    _ => return Err(PlanError::BadArgument { call: "Sample", want: WANT }),
                },
                _ => return Err(PlanError::BadArgument { call: "Sample", want: WANT }),
            }
        }
        let (Some(inner), Some(stride)) = (inner, stride) else {
            return Err(PlanError::Arity { call: "Sample", want: WANT, got: call.args.len() });
        };
        Ok(Rows::Sample { inner: Box::new(inner), stride })
    }

    fn row(&self, call: &Call) -> Result<Rows> {
        // A time window is the only form with more than one argument, so it is recognised
        // before the shapes that insist on exactly one.
        if call.args.len() > 1 {
            return match self.row_between(&call.args)? {
                Some(rows) => Ok(rows),
                None => Err(PlanError::Arity {
                    call: "Row",
                    want: "one comparison, or a key with from= and to=",
                    got: call.args.len(),
                }),
            };
        }
        let (field, op, value) = match call.args.as_slice() {
            [Expr::Compare { field, op, value }] => (field.clone(), op.clone(), value.clone()),
            [Expr::Named { name, value }] => match value.as_ref() {
                Expr::Literal(v) => (name.clone(), "=".to_string(), v.clone()),
                _ => return Err(PlanError::BadArgument { call: "Row", want: "a literal value" }),
            },
            other => {
                return Err(PlanError::Arity {
                    call: "Row",
                    want: "one comparison",
                    got: other.len(),
                })
            }
        };

        let class = self.class(&field)?;
        match (class, &value) {
            // `Sdec` is here only to reach `to_units`, which refuses it by name: a decimal field
            // is unsigned, and the sentence that says so is more use than the operator refusal
            // this would otherwise fall through to.
            (
                FieldClass::Integer { scale },
                Literal::Int(_) | Literal::Dec { .. } | Literal::Sdec { .. },
            ) => {
                let cmp = int_op(&op).ok_or_else(|| PlanError::OperatorNotAllowed {
                    field: field.clone(),
                    op: op.clone(),
                    class: class_name(class),
                })?;
                let value = to_units(&field, &value, scale)?;
                Ok(Rows::Compare { field, op: cmp, value })
            }
            (FieldClass::Signed, Literal::Int(_) | Literal::Sint(_)) => {
                let cmp = int_op(&op).ok_or_else(|| PlanError::OperatorNotAllowed {
                    field: field.clone(),
                    op: op.clone(),
                    class: class_name(class),
                })?;
                let value = match value {
                    Literal::Sint(v) => v,
                    // A positive bound is written the same way for both kinds, so it arrives as
                    // an `Int` and has to survive the trip: refusing one above `i64::MAX` here
                    // rather than truncating keeps `> 2^63` from quietly becoming `> -2^63`.
                    Literal::Int(v) => {
                        i64::try_from(v).map_err(|_| PlanError::NumberTooLarge { at: 0 })?
                    }
                    _ => unreachable!("guarded by the match arm"),
                };
                Ok(Rows::CompareSigned { field, op: cmp, value })
            }
            // Every numeric literal is legal against a float, including the negative fractional
            // one no other class takes. The threshold travels as written; the rounding it needs
            // to become a bound the field can hold depends on the operator *and* the field's
            // width, and the only layer that knows the width is the one that stores it.
            (
                FieldClass::Float { .. },
                Literal::Int(_) | Literal::Sint(_) | Literal::Dec { .. } | Literal::Sdec { .. },
            ) => {
                let cmp = int_op(&op).ok_or_else(|| PlanError::OperatorNotAllowed {
                    field: field.clone(),
                    op: op.clone(),
                    class: class_name(class),
                })?;
                let bits = value.as_f64().expect("guarded by the match arm").to_bits();
                Ok(Rows::CompareFloat { field, op: cmp, bits })
            }
            // A date is compared against a written date, and comes out as the ordinary signed
            // comparison it is: the stored value is a count from the epoch, so `Rows` needs no
            // variant of its own and neither does anything below it.
            (FieldClass::Temporal { unit }, Literal::Str(s)) => {
                let cmp = int_op(&op).ok_or_else(|| PlanError::OperatorNotAllowed {
                    field: field.clone(),
                    op: op.clone(),
                    class: class_name(class),
                })?;
                match unit.to_count(s) {
                    Some(value) => Ok(Rows::CompareSigned { field, op: cmp, value }),
                    None => {
                        Err(PlanError::BadDate { field, written: s.clone(), want: unit.format() })
                    }
                }
            }
            (FieldClass::Keyed(_), Literal::Str(v)) if op == "=" || op == "==" => {
                Ok(Rows::Key { field, value: v.clone() })
            }
            (FieldClass::Boolean, Literal::Bool(v)) if op == "=" || op == "==" => {
                Ok(Rows::Bool { field, value: *v })
            }
            (class, _) => {
                Err(PlanError::OperatorNotAllowed { field, op, class: class_name(class) })
            }
        }
    }
}

/// A named argument whose value must be a plain string, e.g. `field="amount"`.
fn str_arg(call: &'static str, value: &Expr) -> Result<String> {
    match value {
        Expr::Literal(Literal::Str(s)) => Ok(s.clone()),
        // A bare word reads as a name, which is what someone writing `field=amount` meant.
        Expr::Ident(s) => Ok(s.clone()),
        _ => Err(PlanError::BadArgument { call, want: "a field name" }),
    }
}

pub(crate) fn int_op(op: &str) -> Option<CmpOp> {
    Some(match op {
        ">" => CmpOp::Gt,
        ">=" => CmpOp::Ge,
        "<" => CmpOp::Lt,
        "<=" => CmpOp::Le,
        "=" | "==" => CmpOp::Eq,
        "!=" => CmpOp::Ne,
        _ => return None,
    })
}

/// Whether a calendar boundary says anything about a value this field could hold.
///
/// **One function because two layers ask it.** A `date_trunc` in a select list is checked when
/// the answer's shape is resolved, and one in a `GROUP BY` is checked when the plan is built;
/// they are the same question, and two copies of the sentence would eventually disagree about
/// which boundaries a `DATE` has.
pub fn check_boundary(field: &str, class: FieldClass, unit: big_civil::Unit) -> Result<()> {
    match class {
        // Truncating a count of whole days to the hour asks about a time of day the column never
        // held, and answering it would hand back the same date wearing a precision it does not
        // have.
        FieldClass::Temporal { unit: TimeUnit::Days } if !unit.is_whole_days() => {
            Err(PlanError::BadRounding {
                call: format!("a boundary below a day, on `{field}`"),
                why: "a boundary of a day or coarser: a DATE counts whole days, and there is no \
                      time of day in one to round",
            })
        }
        FieldClass::Temporal { .. } => Ok(()),
        other => Err(PlanError::BadRounding {
            call: format!("a calendar boundary on `{field}`"),
            why: match other {
                FieldClass::Keyed(_) => "a DATE or DATETIME column, and this one holds keys",
                _ => "a DATE or DATETIME column, and this one holds neither",
            },
        }),
    }
}

/// Rewrites a written number into the units the field actually stores.
///
/// `price > 5` on a field with two decimal places means `> 500`, not `> 5`. Getting this wrong
/// is not a rounding error, it is off by a factor of a hundred, and nothing downstream could
/// notice because both numbers are valid.
///
/// Public because a `HAVING` threshold is the same conversion asked one layer up: SQL compares
/// a written number against an aggregate that is stored in units, and a second implementation
/// of this is the one that would drift. The caller there is `big-sql`'s `Shape`.
pub fn to_units(field: &str, value: &Literal, scale: u8) -> Result<u64> {
    let (units, written) = match value {
        Literal::Int(v) => (*v, 0u8),
        Literal::Dec { units, scale } => (*units, *scale),
        // The refusal the lexer used to make, moved here now that a negative fractional number
        // has somewhere else to go. This is the arm that knows the value was aimed at an
        // unsigned bit-sliced field, which is the whole reason it cannot be stored: rounding it
        // to an integer would answer a different question.
        Literal::Sdec { .. } => return Err(PlanError::NegativeDecimal { at: 0 }),
        _ => return Err(PlanError::BadArgument { call: "Row", want: "a number" }),
    };

    if written > scale {
        // Silently dropping digits would answer a question the user did not ask.
        return Err(PlanError::TooPrecise { field: field.to_string(), written, scale });
    }
    let shift = scale - written;
    10u64
        .checked_pow(shift as u32)
        .and_then(|m| units.checked_mul(m))
        .ok_or(PlanError::NumberTooLarge { at: 0 })
}

/// Whether an aggregate is meaningful over a field of this class.
///
/// **One function because there are two gates**, and they did not agree. `Sum(field="balance")`
/// on its own was answered and the same call inside a `GroupBy` was refused, because the bare
/// form's check listed `Signed` and the grouped one did not - a refusal nobody wrote on purpose,
/// and one the executor never needed, since it routes a signed sum by the field either way.
/// Written out here so the two cannot drift again.
///
/// `min` and `max` are questions about an ordering; `sum` is a question about addition. A date
/// has the first and not the second, which is the whole of why it is listed separately.
fn aggregable(class: FieldClass, name: &str) -> bool {
    match class {
        FieldClass::Integer { .. } | FieldClass::Signed | FieldClass::Float { .. } => true,
        FieldClass::Temporal { .. } => name == "Min" || name == "Max",
        FieldClass::Keyed(_) | FieldClass::Boolean => false,
    }
}

pub(crate) fn class_name(c: FieldClass) -> &'static str {
    match c {
        FieldClass::Integer { .. } => "an integer field",
        FieldClass::Signed => "a signed integer field",
        FieldClass::Float { bits: 32 } => "a single precision float field",
        FieldClass::Float { .. } => "a float field",
        FieldClass::Temporal { unit: TimeUnit::Days } => "a date field",
        FieldClass::Temporal { unit: TimeUnit::Seconds } => "a timestamp field",
        FieldClass::Keyed(Keyed::Set) => "a keyed field",
        FieldClass::Keyed(Keyed::Mutex) => "a mutex field",
        FieldClass::Keyed(Keyed::Time) => "a time quantum field",
        FieldClass::Boolean => "a boolean field",
    }
}
