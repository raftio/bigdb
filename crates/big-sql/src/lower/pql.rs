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

//! Building the query language this crate emits.
//!
//! Small on purpose. **The only thing the lowering can produce is PQL**, which is what bounds
//! what a statement is permitted to say by what the planner will resolve - a property of the
//! type rather than a discipline anyone keeps.

use crate::ast::Name;
use big_plan::ast::{Call, Expr, Literal};

pub(super) fn row(field: &Name, op: &str, value: Literal) -> Expr {
    // Always a `Compare`, including for `=`. The query language's own parser produces a `Named`
    // there because `field=amount` and `country="GB"` are the same syntax and it refuses to
    // guess between them; nothing is ambiguous on this path, so the resolved shape is written
    // directly. The planner accepts both and treats them identically.
    Expr::Call(Call {
        name: "Row".to_string(),
        args: vec![Expr::Compare { field: field.column.clone(), op: op.to_string(), value }],
    })
}

/// `Row(<field>="<key>", from=<seconds>, to=<seconds>)`, the window a time quantum field
/// answers from its day views.
///
/// An absent bound is genuinely absent rather than a very large number: standing in a sentinel
/// would push a date conversion out to the limits of the calendar for no reason.
pub(super) fn window(field: &Name, key: &str, from: Option<u64>, to: Option<u64>) -> Expr {
    let mut args = vec![Expr::Named {
        name: field.column.clone(),
        value: Box::new(Expr::Literal(Literal::Str(key.to_string()))),
    }];
    for (name, v) in [("from", from), ("to", to)] {
        if let Some(v) = v {
            args.push(Expr::Named {
                name: name.to_string(),
                value: Box::new(Expr::Literal(Literal::Int(v))),
            });
        }
    }
    Expr::Call(Call { name: "Row".to_string(), args })
}

pub(super) fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Call(call_of(name, args))
}

pub(super) fn call_of(name: &str, args: Vec<Expr>) -> Call {
    Call { name: name.to_string(), args }
}

pub(super) fn as_expr(c: Call) -> Expr {
    Expr::Call(c)
}

/// An [`Expr`] known to be a call, unwrapped.
///
/// Every constructor above builds one, so this cannot fail; it exists because a `Statement`
/// carries the outermost call rather than an expression, which is the shape
/// [`big_plan::plan`] takes.
pub(super) fn as_call(e: Expr) -> Call {
    match e {
        Expr::Call(c) => c,
        _ => unreachable!("every condition lowers to a call"),
    }
}

pub(super) fn field_arg(field: &Name) -> Expr {
    named("field", Expr::Ident(field.column.clone()))
}

pub(super) fn named(name: &str, value: Expr) -> Expr {
    Expr::Named { name: name.to_string(), value: Box::new(value) }
}
