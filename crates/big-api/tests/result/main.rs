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

//! Applying a shape to the answers its plans produced - one module per thing being claimed.
//!
//! These run with no pager, no socket and no query text. A shape and a list of `Value`s is
//! precisely what a coordinator holds at the moment it assembles a row, so building both by
//! hand tests the arithmetic rather than the plumbing around it. Until this module existed the
//! arithmetic had no test that was not an HTTP request.

mod common;

mod absence;
mod cuts;
mod joins;
mod shapes;
