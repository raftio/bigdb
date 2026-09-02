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

//! The untyped shape of a query, straight out of the parser.
//!
//! Nothing here has been checked against a schema: a field name is a string and a value is
//! whatever was written. Resolution happens in [`mod@crate::plan`], which is what keeps a parse
//! error and a schema error from being the same error.

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Literal {
    Int(u64),
    /// A number written with a leading `-`.
    ///
    /// A separate variant rather than widening `Int`, because the two are not interchangeable
    /// at the boundary: `-1` against an unsigned field is a mistake worth naming, and folding
    /// both into an `i64` would lose every value above `i64::MAX` to gain nothing.
    Sint(i64),
    /// `12.50` as `units = 1250, scale = 2`.
    ///
    /// Kept exactly as written instead of as a float: a decimal field stores an integer, and
    /// going through `f64` on the way there would introduce error the storage layer does not
    /// have and cannot correct.
    Dec {
        units: u64,
        scale: u8,
    },
    /// `-12.50` as `units = -1250, scale = 2`.
    ///
    /// Its own variant for the reason [`Literal::Sint`] is, and it exists at all because floats
    /// do. A decimal field is unsigned, so this used to be refused in the lexer; a float field
    /// holds negative numbers perfectly well, and a lexer cannot see which kind of field a
    /// value is headed for. So the shape is read here and the refusal moved to [`crate::plan`],
    /// where the field is known - which is where it always belonged.
    ///
    /// Still integers, so [`Literal`] keeps its `Eq`. Nothing in this crate holds an `f64`.
    Sdec {
        units: i64,
        scale: u8,
    },
    Str(String),
    Bool(bool),
}

impl Literal {
    /// The number this stands for, as the float a float field would store.
    ///
    /// `None` for a value that is not a number at all. The division is exact for every scale a
    /// literal can carry, and it is the same division `f64::from_str` would have done - which is
    /// why a written `3.14` and a parsed `3.14` are the same `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        Some(match *self {
            Self::Int(v) => v as f64,
            Self::Sint(v) => v as f64,
            Self::Dec { units, scale } => units as f64 / 10f64.powi(i32::from(scale)),
            Self::Sdec { units, scale } => units as f64 / 10f64.powi(i32::from(scale)),
            Self::Str(_) | Self::Bool(_) => return None,
        })
    }
}

/// `Name(arg, arg, ...)`, the only syntactic form the language has.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Call {
    pub name: String,
    pub args: Vec<Expr>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Expr {
    Call(Call),
    /// `field op value`, only legal inside `Row(...)`.
    Compare {
        field: String,
        op: String,
        value: Literal,
    },
    Literal(Literal),
    /// `name = value` as an argument, e.g. `field="amount"` or `aggregate=Sum(...)`.
    ///
    /// The value is a whole expression rather than a literal so an argument can be another
    /// call, which is what `GroupBy(..., aggregate=Sum(field="amount"))` needs.
    Named {
        name: String,
        value: Box<Expr>,
    },
    Ident(String),
}
