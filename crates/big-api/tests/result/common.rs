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

//! Building an answer by hand, which is the whole point of these tests.
//!
//! No pager, no server and no query text: a shape and a list of `Value`s is exactly what the
//! coordinator holds when it applies the shape, so that is what a test holds. Everything here
//! used to be reachable only through a socket.

use big_api::{
    Answer, Cell, Container, Format, Group, Matches, Of, Pair, RowSet, Selected, Shape, Units,
    Value,
};

/// A statement's answer, in the default format and with no searches.
pub fn answer(shape: Shape) -> Answer {
    Answer { shape, format: Format::Json, calls: usize::MAX }
}

/// A statement's answer whose calls end where its searches begin.
pub fn answer_with_probes(shape: Shape, calls: usize) -> Answer {
    Answer { shape, format: Format::Json, calls }
}

/// One named column reading one thing.
pub fn cell(column: &str, of: Of) -> Cell {
    Cell::plain(column, of)
}

/// One group: a row id, the key it was interned from, and its number.
pub fn group(row: u64, key: Option<&str>, value: Value) -> Group {
    Group { row, key: key.map(str::to_string), value: Box::new(value) }
}

/// One pair of groups, which is what `GROUP BY a, b` answers with.
pub fn pair(left: Group, right: Group) -> Pair {
    Pair { left, right }
}

/// A plan that answered with groups.
pub fn groups(gs: Vec<Group>) -> Value {
    Value::Groups(gs)
}

/// The records of shard zero, as the row set a `Rows` answer carries.
///
/// The existence row is row zero, which is where a set of matching records lives.
pub fn matching(records: &[u16]) -> Matches {
    let mut rows = RowSet::new();
    rows.insert(0, Container::from_values(records.iter().copied()));
    let mut m = Matches::new();
    m.insert(0, rows);
    m
}

/// One projected column whose field stores whole numbers, which is every field but a decimal.
pub fn plain_column(column: &str) -> Selected {
    Selected { column: column.to_string(), units: Units::PLAIN, apply: None }
}

/// One projected column out of a decimal field of `scale` digits.
pub fn scaled_column(column: &str, scale: u8) -> Selected {
    Selected { column: column.to_string(), units: Units::Digits(scale), apply: None }
}
