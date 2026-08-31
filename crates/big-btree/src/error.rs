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

use big_page::{PageError, PageType, Pgno};
use big_pager::StoreError;

#[derive(Debug)]
pub enum BTreeError {
    Store(StoreError),
    Page(PageError),
    UnexpectedPage {
        pgno: Pgno,
        found: PageType,
    },
    EmptyBranch {
        pgno: Pgno,
    },
    /// A `child` pointer read off disk led into a walk deeper than any real tree.
    TooDeep {
        root: Pgno,
    },
    /// A container too large to sit inline was handed over without a page to promote it onto.
    PayloadTooLarge {
        bytes: usize,
    },
}

impl From<StoreError> for BTreeError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<PageError> for BTreeError {
    fn from(e: PageError) -> Self {
        Self::Page(e)
    }
}

/// Everything below `Store` and `Page` means the tree on disk is not shaped the way the tree
/// in this build expects, which in practice means damage rather than a caller's mistake.
impl core::fmt::Display for BTreeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Page(e) => write!(f, "{e}"),
            Self::UnexpectedPage { pgno, found } => {
                write!(f, "page {pgno} is a {found:?} page where the tree expected a node")
            }
            Self::EmptyBranch { pgno } => {
                write!(f, "branch page {pgno} has no children")
            }
            Self::TooDeep { root } => {
                write!(f, "the tree at page {root} is deeper than any real tree; it is damaged")
            }
            Self::PayloadTooLarge { bytes } => {
                write!(f, "a {bytes}-byte container was not given a page to live on")
            }
        }
    }
}

/// See [`big_page::PageError::code`].
///
/// The three shape variants share `tree_damaged` for the same reason the page ones share
/// `page_damaged`: they are different symptoms of one diagnosis and one remedy.
impl BTreeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Store(e) => e.code(),
            Self::Page(e) => e.code(),
            Self::UnexpectedPage { .. } | Self::EmptyBranch { .. } | Self::TooDeep { .. } => {
                "tree_damaged"
            }
            Self::PayloadTooLarge { .. } => "payload_too_large",
        }
    }
}

impl core::error::Error for BTreeError {}

pub type Result<T> = core::result::Result<T, BTreeError>;
