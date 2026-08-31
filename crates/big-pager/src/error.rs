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

use big_page::{PageError, Pgno};

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Page(PageError),
    /// A read past EOF. Inside a reserved mapping, touching that region would be a SIGBUS.
    OutOfBounds {
        pgno: Pgno,
        page_count: u64,
    },
    /// The file needs to outgrow the reserved `mapsize`. A hard error, never a silent remap:
    /// remapping is exactly what would turn every live borrow into a dangling one.
    MapSizeExhausted {
        need: u64,
        mapsize: u64,
    },
    /// Another handle holds the file. The exclusive lock is a soundness requirement of the
    /// mapping, not a convenience.
    Locked,
    /// Both meta pages are unreadable.
    NoValidMeta,
    /// A file with something in it, but too little to be a database.
    ///
    /// Told apart from an empty path on purpose. An empty path is a database that has not been
    /// created yet and is created on the spot; this is somebody else's file, and initialising it
    /// would destroy it. The distinction is the whole point of the variant - without it the
    /// two are one branch, and the branch that wins is the destructive one.
    NotADatabase {
        /// What the file holds, so the message can say why it is not a database.
        bytes: u64,
    },
    /// A `next` chain read off disk loops; caught before it becomes an infinite walk.
    ChainCycle {
        root: Pgno,
    },
    SnapshotNotFound(u64),
    /// A write aimed at a page this transaction never allocated.
    UnallocatedPage(Pgno),
    /// Shrinking the file is refused while a reader could still be borrowing into it.
    ReadersActive,
    Unsupported(&'static str),
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<PageError> for StoreError {
    fn from(e: PageError) -> Self {
        Self::Page(e)
    }
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Page(e) => write!(f, "{e}"),
            Self::OutOfBounds { pgno, page_count } => {
                write!(f, "page {pgno} is past the end of a {page_count}-page file")
            }
            Self::MapSizeExhausted { need, mapsize } => write!(
                f,
                "the file needs {need} pages but the mapping reserves {mapsize}; \
                 reopen with a larger mapsize"
            ),
            Self::Locked => {
                write!(f, "another process holds this file; only one may have it open")
            }
            // Deliberately points at the backup rather than at a repair: with no write-ahead
            // log there is no half-applied state a repair could reason about, so a file that
            // fails both meta pages was damaged by something outside the engine.
            Self::NoValidMeta => write!(
                f,
                "neither meta page is readable; this file is damaged - restore from a backup"
            ),
            Self::NotADatabase { bytes } => write!(
                f,
                "this file is {bytes} bytes, which is too small to be a big database and too \
                 large to be an empty path; refusing to overwrite it"
            ),
            Self::ChainCycle { root } => {
                write!(f, "the metadata chain starting at page {root} loops back on itself")
            }
            Self::SnapshotNotFound(id) => write!(f, "no snapshot with id {id}"),
            Self::UnallocatedPage(pgno) => {
                write!(f, "a write named page {pgno}, which this transaction never allocated")
            }
            Self::ReadersActive => {
                write!(f, "readers are still active; shrinking the file has to wait for them")
            }
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
        }
    }
}

/// See [`big_page::PageError::code`] for why these exist alongside `Display`.
///
/// `Page` delegates rather than flattening: a checksum mismatch reaching an operator through
/// the store is still a checksum mismatch, and wrapping it in a `storage` code would hide the
/// one detail that decides what to do about it.
impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Page(e) => e.code(),
            Self::OutOfBounds { .. } => "page_out_of_bounds",
            Self::MapSizeExhausted { .. } => "mapsize_exhausted",
            Self::Locked => "file_locked",
            // Both mean the bytes on disk are not what this build wrote, and both are answered
            // by a restore.
            Self::NoValidMeta | Self::ChainCycle { .. } => "file_damaged",
            Self::SnapshotNotFound(_) => "snapshot_not_found",
            Self::UnallocatedPage(_) => "unallocated_page",
            Self::NotADatabase { .. } => "not_a_database",
            Self::ReadersActive => "readers_active",
            Self::Unsupported(_) => "unsupported",
        }
    }
}

impl core::error::Error for StoreError {}

pub type Result<T> = core::result::Result<T, StoreError>;
