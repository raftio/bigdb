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

//! `WHERE round(price, 2) > 5`, as the range of stored values that satisfy it.
//!
//! # Why this is here and not in the SQL parser
//!
//! A rounded *date* is rewritten where it is written: its two bounds are written dates, and a
//! written date means the same thing to a column counting days and to one counting seconds, so
//! one rewrite serves both and needs no schema. A rounded *number* is not like that. All four of
//! these are the same statement over a different column:
//!
//! ```text
//! round(x, 2) > 5   on DECIMAL(10,2)  ->  x >= 5.01
//! round(x, 2) > 5   on DECIMAL(10,4)  ->  x >= 5.005
//! round(x, 2) > 5   on INT            ->  x >= 6
//! round(x, 2) > 5   on SIGNED         ->  x >  5      (rounding an integer is the identity)
//! ```
//!
//! Which one it is depends on the field's scale, and the scale lives here. Done in the units the
//! field stores, every bound above comes out an **exact integer** - so there is no threshold the
//! column cannot hold and nothing to round in a direction somebody has to reason about.
//!
//! # The shape of the arithmetic
//!
//! Each rounding lands its answers on a grid of step `g`, in the field's own units:
//!
//! | rounding | grid step | the values that map to grid point `M` |
//! |---|---|---|
//! | `floor` | one whole number | `[M, M + g)` |
//! | `ceil` | one whole number | `(M - g, M]` |
//! | `round(k)` | `10^-k` | `[M - g/2, M + g/2)` |
//!
//! `g` is a power of ten times the field's own unit, so `g/2` is exact whenever `g > 1`. When the
//! grid is finer than the column - `round(x, 4)` on a `DECIMAL(10,2)`, or any rounding of an
//! integer - the rounding is the identity and the comparison is passed through unchanged.

use crate::error::{PlanError, Result};
use crate::plan::{CmpOp, Rows};
use crate::schema::FieldClass;
use crate::Literal;

/// Which rounding a `Rounded(...)` call names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum By {
    /// `round(x, digits)`, half away from zero. Every field this reaches with a grid coarser
    /// than one unit is unsigned, so "away from zero" and "up" are the same rule here.
    Round { digits: u8 },
    /// `floor(x)`
    Floor,
    /// `ceil(x)`
    Ceil,
}

impl By {
    /// The spelling `big-sql` sends, and the one the query language takes.
    pub fn parse(s: &str, digits: Option<u8>) -> Option<Self> {
        Some(match s {
            "round" => Self::Round { digits: digits.unwrap_or(0) },
            "floor" => Self::Floor,
            "ceil" => Self::Ceil,
            _ => return None,
        })
    }

    fn written(self) -> String {
        match self {
            Self::Round { digits } => format!("round(x, {digits})"),
            Self::Floor => "floor(x)".to_string(),
            Self::Ceil => "ceil(x)".to_string(),
        }
    }
}

/// The records whose value, rounded, compares to `value` the way `op` says.
pub(crate) fn rows(
    field: &str,
    class: FieldClass,
    by: By,
    op: &str,
    value: &Literal,
) -> Result<Rows> {
    match class {
        // The only class with a scale, and so the only one where a rounding is not the identity.
        FieldClass::Integer { scale } => {
            let units = crate::plan::to_units(field, value, scale)?;
            let grid = grid(by, scale);
            match grid {
                // Finer than the column stores: every stored value is already on the grid.
                1 => Ok(Rows::Compare {
                    field: field.to_string(),
                    op: int_op(field, op, class)?,
                    value: units,
                }),
                g => bounded(field, by, op, i128::from(units), i128::from(g), Bound::Unsigned),
            }
        }
        // Whole numbers, so all three roundings are the identity - and saying so is the whole
        // work: `round(balance, 2) > 5` is `balance > 5`, not a range around it.
        FieldClass::Signed => {
            let v = match value {
                Literal::Sint(v) => *v,
                Literal::Int(v) => {
                    i64::try_from(*v).map_err(|_| PlanError::NumberTooLarge { at: 0 })?
                }
                _ => {
                    return Err(PlanError::BadArgument { call: "Rounded", want: "a whole number" })
                }
            };
            Ok(Rows::CompareSigned {
                field: field.to_string(),
                op: int_op(field, op, class)?,
                value: v,
            })
        }
        // A float has no scale to work in, so only the roundings whose bounds are whole numbers
        // are exact: `floor` and `ceil` always, `round(x, 0)` because a half is exact in binary
        // too. Anything finer would need `10^-k`, which is not a number a float holds exactly,
        // and a bound off by one place is a wrong answer rather than an imprecise one.
        FieldClass::Float { .. } => {
            if matches!(by, By::Round { digits } if digits > 0) {
                return Err(PlanError::BadRounding {
                    call: by.written(),
                    why: "a whole number of digits on a float, or a DECIMAL column: a tenth is \
                          not a number a float holds exactly, so the bound would be too",
                });
            }
            let v = value
                .as_f64()
                .ok_or(PlanError::BadArgument { call: "Rounded", want: "a number" })?;
            float(field, by, op, v)
        }
        other => Err(PlanError::OperatorNotAllowed {
            field: field.to_string(),
            op: by.written(),
            class: crate::plan::class_name(other),
        }),
    }
}

/// The step a rounding's answers land on, in the field's units. `1` means the identity.
fn grid(by: By, scale: u8) -> u64 {
    let whole = 10u64.saturating_pow(u32::from(scale));
    match by {
        By::Floor | By::Ceil => whole,
        // Keeping `digits` places of a column that stores `scale` of them leaves the last
        // `scale - digits` to round away. Keeping as many or more leaves nothing.
        By::Round { digits } if u32::from(digits) < u32::from(scale) => {
            10u64.saturating_pow(u32::from(scale) - u32::from(digits))
        }
        By::Round { .. } => 1,
    }
}

/// Whether the field's stored values can go below zero, which decides what an underflowing
/// bound means.
enum Bound {
    Unsigned,
}

/// The comparison, worked out on the grid.
fn bounded(field: &str, by: By, op: &str, value: i128, g: i128, _sign: Bound) -> Result<Rows> {
    // The smallest stored value whose rounding reaches grid point `m`.
    let edge = |m: i128| match by {
        By::Floor => m,
        // `ceil(x) >= m` is `x > m - g`, and the first value above that is one unit up.
        By::Ceil => m - g + 1,
        By::Round { .. } => m - g / 2,
    };
    // The grid points either side of the value compared against.
    let at_or_above = value.div_euclid(g) * g + if value.rem_euclid(g) == 0 { 0 } else { g };
    let above = value.div_euclid(g) * g + g;

    let ge = |bound: i128| match u64::try_from(bound.max(0)) {
        Ok(v) => Rows::Compare { field: field.to_string(), op: CmpOp::Ge, value: v },
        Err(_) => unreachable!("clamped at zero"),
    };
    // An upper bound at or below zero selects nothing, and `Not(All)` is how this layer spells
    // the empty set - there is no `Rows` variant for it because no other term needs one.
    let lt = |bound: i128| match bound <= 0 {
        true => Rows::Not(Box::new(Rows::All)),
        false => Rows::Compare {
            field: field.to_string(),
            op: CmpOp::Lt,
            value: u64::try_from(bound).unwrap_or(u64::MAX),
        },
    };

    Ok(match op {
        ">=" => ge(edge(at_or_above)),
        ">" => ge(edge(above)),
        "<" => lt(edge(at_or_above)),
        "<=" => lt(edge(above)),
        "=" | "==" | "!=" => {
            if value.rem_euclid(g) != 0 {
                return Err(PlanError::BadRounding {
                    call: by.written(),
                    why: "a value it can produce: this one lies between two of them, so nothing \
                          could round to it",
                });
            }
            let within = Rows::Intersect(vec![ge(edge(value)), lt(edge(value + g))]);
            match op {
                "!=" => Rows::Not(Box::new(within)),
                _ => within,
            }
        }
        _ => {
            return Err(PlanError::BadArgument {
                call: "Rounded",
                want: "one of =, !=, <, <=, > or >=",
            })
        }
    })
}

/// The same question over a float, where the grid is whole numbers and the bounds are exact.
fn float(field: &str, by: By, op: &str, value: f64) -> Result<Rows> {
    let bits = |v: f64| v.to_bits();
    let cmp =
        |op: CmpOp, v: f64| Rows::CompareFloat { field: field.to_string(), op, bits: bits(v) };
    // The grid is the whole numbers, and `round(x, 0)`'s cell reaches half a unit either side.
    let edge = |m: f64| match by {
        By::Floor => m,
        By::Ceil => m - 1.0,
        By::Round { .. } => m - 0.5,
    };
    // `ceil` is the one whose cell is open at the bottom, so its bound is strict.
    let low = |m: f64| match by {
        By::Ceil => cmp(CmpOp::Gt, edge(m)),
        _ => cmp(CmpOp::Ge, edge(m)),
    };
    let high = |m: f64| match by {
        By::Ceil => cmp(CmpOp::Le, edge(m)),
        _ => cmp(CmpOp::Lt, edge(m)),
    };
    let at_or_above = value.ceil();
    let above = value.floor() + 1.0;

    Ok(match op {
        ">=" => low(at_or_above),
        ">" => low(above),
        "<" => high(at_or_above),
        "<=" => high(above),
        "=" | "==" | "!=" => {
            if value.fract() != 0.0 {
                return Err(PlanError::BadRounding {
                    call: by.written(),
                    why: "a whole number: a rounding to whole numbers produces nothing else",
                });
            }
            let within = Rows::Intersect(vec![low(value), high(value + 1.0)]);
            match op {
                "!=" => Rows::Not(Box::new(within)),
                _ => within,
            }
        }
        _ => {
            return Err(PlanError::BadArgument {
                call: "Rounded",
                want: "one of =, !=, <, <=, > or >=",
            })
        }
    })
}

/// The comparison operator, refused by name where the class has no ordering for it.
fn int_op(field: &str, op: &str, class: FieldClass) -> Result<CmpOp> {
    crate::plan::int_op(op).ok_or_else(|| PlanError::OperatorNotAllowed {
        field: field.to_string(),
        op: op.to_string(),
        class: crate::plan::class_name(class),
    })
}
