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

//! Comparisons, row expressions and plans.
//!
//! The *question* half of the protocol, and the recursive half: `Rows` nests and `Plan` holds
//! `Rows`, so both decoders carry a depth and refuse past [`MAX_DEPTH`]. A peer that could
//! nest without bound could overflow this node's stack from the other side of a socket.

use super::*;

// ---------------------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------------------

fn put_cmp(out: &mut Vec<u8>, op: CmpOp) {
    put_u8(
        out,
        match op {
            CmpOp::Gt => 0,
            CmpOp::Ge => 1,
            CmpOp::Lt => 2,
            CmpOp::Le => 3,
            CmpOp::Eq => 4,
            CmpOp::Ne => 5,
        },
    );
}

fn get_cmp(r: &mut Reader<'_>) -> Result<CmpOp> {
    Ok(match r.u8()? {
        0 => CmpOp::Gt,
        1 => CmpOp::Ge,
        2 => CmpOp::Lt,
        3 => CmpOp::Le,
        4 => CmpOp::Eq,
        5 => CmpOp::Ne,
        tag => return Err(WireError::BadTag { what: "comparison", tag }),
    })
}

mod rows_tag {
    pub const COMPARE: u8 = 0;
    pub const COMPARE_SIGNED: u8 = 1;
    pub const KEY: u8 = 2;
    pub const KEY_BETWEEN: u8 = 3;
    pub const BOOL: u8 = 4;
    pub const INTERSECT: u8 = 5;
    pub const UNION: u8 = 6;
    pub const DIFFERENCE: u8 = 7;
    pub const NOT: u8 = 8;
    pub const ALL: u8 = 9;
    pub const COMPARE_FLOAT: u8 = 10;
    pub const KEY_LIKE: u8 = 11;
}

pub fn put_rows(out: &mut Vec<u8>, rows: &Rows) {
    match rows {
        Rows::Compare { field, op, value } => {
            put_u8(out, rows_tag::COMPARE);
            put_str(out, field);
            put_cmp(out, *op);
            put_u64(out, *value);
        }
        Rows::CompareSigned { field, op, value } => {
            put_u8(out, rows_tag::COMPARE_SIGNED);
            put_str(out, field);
            put_cmp(out, *op);
            put_i64(out, *value);
        }
        // The threshold is already `f64::to_bits`, so this is bit-exact by construction: no
        // decimal spelling to round-trip, and no rounding decision made anywhere but at the
        // owner that knows the field's width.
        Rows::CompareFloat { field, op, bits } => {
            put_u8(out, rows_tag::COMPARE_FLOAT);
            put_str(out, field);
            put_cmp(out, *op);
            put_u64(out, *bits);
        }
        Rows::Key { field, value } => {
            put_u8(out, rows_tag::KEY);
            put_str(out, field);
            put_str(out, value);
        }
        Rows::KeyLike { field, pattern, fold } => {
            put_u8(out, rows_tag::KEY_LIKE);
            put_str(out, field);
            put_str(out, pattern);
            put_bool(out, *fold);
        }
        Rows::KeyBetween { field, value, from, to } => {
            put_u8(out, rows_tag::KEY_BETWEEN);
            put_str(out, field);
            put_str(out, value);
            put_opt_i64(out, *from);
            put_opt_i64(out, *to);
        }
        Rows::Bool { field, value } => {
            put_u8(out, rows_tag::BOOL);
            put_str(out, field);
            put_bool(out, *value);
        }
        Rows::Intersect(parts) => put_list(out, rows_tag::INTERSECT, parts),
        Rows::Union(parts) => put_list(out, rows_tag::UNION, parts),
        Rows::Difference(a, b) => {
            put_u8(out, rows_tag::DIFFERENCE);
            put_rows(out, a);
            put_rows(out, b);
        }
        Rows::Not(inner) => {
            put_u8(out, rows_tag::NOT);
            put_rows(out, inner);
        }
        Rows::All => put_u8(out, rows_tag::ALL),
    }
}

fn put_list(out: &mut Vec<u8>, tag: u8, parts: &[Rows]) {
    put_u8(out, tag);
    put_count(out, parts.len());
    for part in parts {
        put_rows(out, part);
    }
}

pub fn get_rows(r: &mut Reader<'_>) -> Result<Rows> {
    get_rows_at(r, 0)
}

fn get_rows_at(r: &mut Reader<'_>, depth: usize) -> Result<Rows> {
    if depth >= MAX_DEPTH {
        return Err(WireError::TooDeep);
    }
    let tag = r.u8()?;
    Ok(match tag {
        rows_tag::COMPARE => Rows::Compare { field: r.str()?, op: get_cmp(r)?, value: r.u64()? },
        rows_tag::COMPARE_SIGNED => {
            Rows::CompareSigned { field: r.str()?, op: get_cmp(r)?, value: r.i64()? }
        }
        rows_tag::COMPARE_FLOAT => {
            Rows::CompareFloat { field: r.str()?, op: get_cmp(r)?, bits: r.u64()? }
        }
        rows_tag::KEY => Rows::Key { field: r.str()?, value: r.str()? },
        rows_tag::KEY_LIKE => Rows::KeyLike { field: r.str()?, pattern: r.str()?, fold: r.bool()? },
        rows_tag::KEY_BETWEEN => Rows::KeyBetween {
            field: r.str()?,
            value: r.str()?,
            from: r.opt_i64()?,
            to: r.opt_i64()?,
        },
        rows_tag::BOOL => Rows::Bool { field: r.str()?, value: r.bool()? },
        rows_tag::INTERSECT => Rows::Intersect(get_list(r, depth)?),
        rows_tag::UNION => Rows::Union(get_list(r, depth)?),
        rows_tag::DIFFERENCE => Rows::Difference(
            Box::new(get_rows_at(r, depth + 1)?),
            Box::new(get_rows_at(r, depth + 1)?),
        ),
        rows_tag::NOT => Rows::Not(Box::new(get_rows_at(r, depth + 1)?)),
        rows_tag::ALL => Rows::All,
        tag => return Err(WireError::BadTag { what: "row set", tag }),
    })
}

fn get_list(r: &mut Reader<'_>, depth: usize) -> Result<Vec<Rows>> {
    let n = r.count()?;
    let mut parts = Vec::with_capacity(n);
    for _ in 0..n {
        parts.push(get_rows_at(r, depth + 1)?);
    }
    Ok(parts)
}

mod plan_tag {
    pub const ROWS: u8 = 0;
    pub const COUNT: u8 = 1;
    pub const SUM: u8 = 2;
    pub const MIN: u8 = 3;
    pub const MAX: u8 = 4;
    pub const DISTINCT: u8 = 5;
    pub const TOP_N: u8 = 6;
    pub const GROUP_BY: u8 = 7;
    pub const PROJECT: u8 = 8;
    pub const GROUP_BY_PAIR: u8 = 9;
}

pub fn put_plan(out: &mut Vec<u8>, plan: &Plan) {
    match plan {
        Plan::Rows { table, rows } => {
            put_u8(out, plan_tag::ROWS);
            put_str(out, table);
            put_rows(out, rows);
        }
        Plan::Count { table, rows } => {
            put_u8(out, plan_tag::COUNT);
            put_str(out, table);
            put_rows(out, rows);
        }
        Plan::Sum { table, rows, field } => put_aggregate(out, plan_tag::SUM, table, rows, field),
        Plan::Min { table, rows, field } => put_aggregate(out, plan_tag::MIN, table, rows, field),
        Plan::Max { table, rows, field } => put_aggregate(out, plan_tag::MAX, table, rows, field),
        Plan::Distinct { table, rows, field } => {
            put_aggregate(out, plan_tag::DISTINCT, table, rows, field)
        }
        Plan::TopN { table, rows, field, n } => {
            put_aggregate(out, plan_tag::TOP_N, table, rows, field);
            put_u64(out, *n as u64);
        }
        Plan::GroupBy { table, rows, field, aggregate } => {
            put_aggregate(out, plan_tag::GROUP_BY, table, rows, field);
            put_plan(out, aggregate);
        }
        Plan::GroupByPair { table, rows, left, right, aggregate, left_max } => {
            put_u8(out, plan_tag::GROUP_BY_PAIR);
            put_str(out, table);
            put_rows(out, rows);
            put_str(out, left);
            put_str(out, right);
            put_u64(out, *left_max as u64);
            put_plan(out, aggregate);
        }
        // The only plan whose field list is a list, so it carries a count rather than reusing
        // `put_aggregate`'s single name.
        Plan::Project { table, rows, fields, limit } => {
            put_u8(out, plan_tag::PROJECT);
            put_str(out, table);
            put_rows(out, rows);
            put_count(out, fields.len());
            for f in fields {
                put_str(out, f);
            }
            // No limit travels as the largest count there is, which is what it means: read
            // every match. The frame stays one fixed-width number either way, and a limit of
            // `usize::MAX` and no limit ask the owner for exactly the same work.
            put_u64(out, limit.map_or(u64::MAX, |n| n as u64));
        }
    }
}

fn put_aggregate(out: &mut Vec<u8>, tag: u8, table: &str, rows: &Rows, field: &str) {
    put_u8(out, tag);
    put_str(out, table);
    put_rows(out, rows);
    put_str(out, field);
}

pub fn get_plan(r: &mut Reader<'_>) -> Result<Plan> {
    get_plan_at(r, 0)
}

fn get_plan_at(r: &mut Reader<'_>, depth: usize) -> Result<Plan> {
    if depth >= MAX_DEPTH {
        return Err(WireError::TooDeep);
    }
    let tag = r.u8()?;
    Ok(match tag {
        plan_tag::ROWS => Plan::Rows { table: r.str()?, rows: get_rows(r)? },
        plan_tag::COUNT => Plan::Count { table: r.str()?, rows: get_rows(r)? },
        plan_tag::SUM => {
            let (table, rows, field) = get_aggregate(r)?;
            Plan::Sum { table, rows, field }
        }
        plan_tag::MIN => {
            let (table, rows, field) = get_aggregate(r)?;
            Plan::Min { table, rows, field }
        }
        plan_tag::MAX => {
            let (table, rows, field) = get_aggregate(r)?;
            Plan::Max { table, rows, field }
        }
        plan_tag::DISTINCT => {
            let (table, rows, field) = get_aggregate(r)?;
            Plan::Distinct { table, rows, field }
        }
        plan_tag::TOP_N => {
            let (table, rows, field) = get_aggregate(r)?;
            // A count that does not fit a `usize` is a count no answer could have that many
            // groups for; saturating keeps a 32-bit target from wrapping it into a small one.
            let n = r.u64()?.try_into().unwrap_or(usize::MAX);
            Plan::TopN { table, rows, field, n }
        }
        plan_tag::GROUP_BY => {
            let (table, rows, field) = get_aggregate(r)?;
            Plan::GroupBy { table, rows, field, aggregate: Box::new(get_plan_at(r, depth + 1)?) }
        }
        plan_tag::GROUP_BY_PAIR => {
            let table = r.str()?;
            let rows = get_rows(r)?;
            let left = r.str()?;
            let right = r.str()?;
            let left_max = r.u64()?.try_into().unwrap_or(usize::MAX);
            Plan::GroupByPair {
                table,
                rows,
                left,
                right,
                aggregate: Box::new(get_plan_at(r, depth + 1)?),
                left_max,
            }
        }
        plan_tag::PROJECT => {
            let table = r.str()?;
            let rows = get_rows(r)?;
            let n = r.count()?;
            let mut fields = Vec::with_capacity(n);
            for _ in 0..n {
                fields.push(r.str()?);
            }
            // A limit that does not fit a `usize` is a page no answer could hold; saturating
            // keeps a 32-bit target from wrapping it into a small one, as `TopN` does. The
            // sentinel `u64::MAX` is the full scan `put_plan` wrote.
            let limit = match r.u64()? {
                u64::MAX => None,
                n => Some(n.try_into().unwrap_or(usize::MAX)),
            };
            Plan::Project { table, rows, fields, limit }
        }
        tag => return Err(WireError::BadTag { what: "plan", tag }),
    })
}

fn get_aggregate(r: &mut Reader<'_>) -> Result<(String, Rows, String)> {
    Ok((r.str()?, get_rows(r)?, r.str()?))
}
