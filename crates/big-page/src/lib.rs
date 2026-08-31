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

//! Byte layout, parsing and checksums for big.
//!
//! **Nothing here may panic on malformed input.** This crate sits directly behind `mmap`, so
//! the bytes it parses are whatever is in the file - including whatever a corrupt disk, a
//! truncated write or a hostile file happens to contain. Every entry point that touches those
//! bytes returns a `Result` over [`error::PageError`], and a fuzz target asserts it.
//!
//! Five page kinds: [`meta`] (recovery is picking the newer valid one of the two - there is no
//! WAL to replay), [`leaf`], [`branch`], [`chain`] for anything that does not fit in one page,
//! and [`record`] for the root records and the catalog. Nothing here opens a file; that is
//! `big-pager`.

#![deny(unsafe_code)]

pub mod branch;
pub mod chain;
pub mod error;
pub mod key;
pub mod layout;
pub mod leaf;
pub mod meta;
pub mod page;
pub mod record;

pub use big_container::{Container, ContainerRef, ContainerType, Interval};
pub use branch::{BranchBuilder, BranchCell, BranchPage};
pub use chain::{build_chain, chain_capacity, chain_pages_needed, ChainPage};
pub use error::PageError;
pub use key::{FragmentKey, RootRecord, FRAGMENT_KEY_BYTES, ROOT_RECORD_BYTES};
pub use layout::*;
pub use leaf::{
    base_words, bitmap_container, bitmap_page_checksum, build_bitmap_page, LeafBuilder, LeafCell,
    LeafPage,
};
pub use meta::{pick_meta, MetaPage};
pub use page::{ContainerKey, Page, PageType, Pgno, TxnId};
pub use record::{kind, ALL_KINDS, CATALOG_ENTRY_BYTES};
