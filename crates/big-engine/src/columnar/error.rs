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

//! What reading or writing a segment can refuse.

/// Every way a block can fail to be what it claims.
///
/// Split from a generic "damaged" for the reason the page layer splits its own: a block written
/// by a newer build and a block that has rotted call for opposite actions, and an error that
/// says only "bad block" leaves an operator unable to tell which they have.
#[derive(Debug)]
pub enum ColumnError {
    Tree(big_btree::BTreeError),
    Page(big_page::PageError),
    /// A block header names an encoding this build has no decoder for.
    UnknownCodec(u8),
    /// A block header names a shape - scalar or list - this build does not have.
    UnknownShape(u8),
    /// The payload is shorter than the header says it needs. Damage, or a truncated write.
    Truncated {
        need: usize,
        have: usize,
    },
    /// A bit width outside `0..=64`, which no encoder produces.
    BadWidth(u8),
    /// A list block needed more parts than the key space reserves. Refused rather than
    /// truncated: a record silently missing values reads back as a record that never had them.
    TooManyParts {
        block: u64,
    },
    /// A cell in a segment carries a set rather than column values. Two trees have been
    /// crossed - the caller is reading a fragment as a segment.
    NotValues(u16),
}

impl From<big_btree::BTreeError> for ColumnError {
    fn from(e: big_btree::BTreeError) -> Self {
        Self::Tree(e)
    }
}

impl From<big_page::PageError> for ColumnError {
    fn from(e: big_page::PageError) -> Self {
        Self::Page(e)
    }
}

impl core::fmt::Display for ColumnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Tree(e) => write!(f, "tree: {e}"),
            Self::Page(e) => write!(f, "page: {e}"),
            Self::UnknownCodec(c) => {
                write!(f, "block encoding {c} is not one this build can read")
            }
            Self::UnknownShape(s) => write!(f, "block shape {s} is not one this build has"),
            Self::Truncated { need, have } => {
                write!(f, "block payload needs {need} bytes and carries {have}")
            }
            Self::BadWidth(w) => write!(f, "bit width {w} is outside 0..=64"),
            Self::TooManyParts { block } => write!(
                f,
                "block {block} holds more values than {} parts can carry",
                crate::columnar::MAX_PARTS
            ),
            Self::NotValues(t) => {
                write!(f, "container type {t} is a set, not a block of column values")
            }
        }
    }
}

impl core::error::Error for ColumnError {}

impl ColumnError {
    /// The stable code this refusal answers with, in the same namespace every other layer
    /// uses.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Tree(e) => e.code(),
            Self::Page(e) => e.code(),
            // All from the file rather than from the caller, and all mean the same thing to an
            // operator: this segment cannot be read by this build.
            Self::UnknownCodec(_) | Self::UnknownShape(_) => "segment_from_newer_build",
            Self::Truncated { .. } | Self::BadWidth(_) | Self::NotValues(_) => "segment_damaged",
            Self::TooManyParts { .. } => "segment_block_too_large",
        }
    }
}

pub type Result<T> = core::result::Result<T, ColumnError>;
