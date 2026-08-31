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

use big_btree::BTreeError;

#[derive(Debug)]
pub enum FieldError {
    Tree(BTreeError),
    /// The value needs more bit planes than this fragment has. `bit_depth` only ever grows, so
    /// the fix is to widen the fragment, never to truncate the value.
    ValueTooWide {
        value: u64,
        bit_depth: u32,
    },
    /// A mutex field found more than one row set for a record, which should be impossible.
    MutexConflict {
        record: u64,
    },
}

impl From<BTreeError> for FieldError {
    fn from(e: BTreeError) -> Self {
        Self::Tree(e)
    }
}

impl core::fmt::Display for FieldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Tree(e) => write!(f, "{e}"),
            Self::ValueTooWide { value, bit_depth } => write!(
                f,
                "{value} needs more than the {bit_depth} bits this field was declared with"
            ),
            // Not a user error: the shadow and the value rows disagree, which the write path
            // is supposed to make impossible.
            Self::MutexConflict { record } => write!(
                f,
                "record {record} holds more than one value in a mutex field, which is a bug"
            ),
        }
    }
}

/// A stable identifier for this failure, in the same scheme as `big_page::PageError::code`.
/// Not a link: `big-page` is a dev-dependency here, so it is not in scope for rustdoc.
impl FieldError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Tree(e) => e.code(),
            Self::ValueTooWide { .. } => "value_too_wide",
            // Not a caller's mistake: the shadow and the value rows disagree, which the write
            // path is supposed to make impossible. It keeps its own code so that it can be
            // alerted on separately from anything a client can provoke.
            Self::MutexConflict { .. } => "mutex_conflict",
        }
    }
}

impl core::error::Error for FieldError {}

pub type Result<T> = core::result::Result<T, FieldError>;
