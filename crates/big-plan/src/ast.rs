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
    Str(String),
    Bool(bool),
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
