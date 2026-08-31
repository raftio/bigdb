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

//! Turning query text into something an executor can run, without touching storage.
//!
//! This crate deliberately depends on no other `big` crate. What a planner needs to know about
//! the database is narrow — does this field exist, and what class of thing is it — so that
//! knowledge arrives through [`Schema`], which the storage side implements. The dependency
//! points *towards* here, which is what keeps parser tests running at unit-test speed with no
//! file, no mapping, and no pager.

#![deny(unsafe_code)]

pub mod ast;
pub mod error;
pub mod parse;
pub mod plan;
pub mod schema;

pub use ast::{Call, Expr, Literal};
pub use error::{PlanError, Result};
pub use parse::parse;
pub use plan::{plan, to_units, CmpOp, Plan, Rows};
pub use schema::{FieldClass, Keyed, Schema};
