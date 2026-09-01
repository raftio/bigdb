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

//! The SQL surface, translated - one module per thing being claimed.
//!
//! Every test here runs with no file, no mapping and no pager: `translate` needs no schema, and
//! the `Stub` in `common` is thirty lines of trait impl. That is what keeps an equivalence test
//! costing what a parser test costs.

mod common;

mod clauses;
mod introspect;
mod joins;
mod refusals;
mod schema;
mod shapes;
mod writes;
