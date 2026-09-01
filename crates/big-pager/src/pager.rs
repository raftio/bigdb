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

//! The storage boundary. Everything above knows only these two traits, not mmap or files.

use crate::error::Result;
use crate::io::IoStats;
use big_page::{Page, Pgno};
use core::ops::Deref;

/// Read path. The GAT lets an implementation return a guard instead of a bare reference,
/// which is exactly what a buffer pool would need.
pub trait Pager {
    type Ref<'a>: Deref<Target = Page>
    where
        Self: 'a;

    fn read(&self, pgno: Pgno) -> Result<Self::Ref<'_>>;

    /// Pages actually present in the file. Reading past this is `OutOfBounds`, not a SIGBUS.
    fn page_count(&self) -> u64;

    /// Hard ceiling of the backend; `None` means only the disk limits it.
    fn capacity(&self) -> Option<u64> {
        None
    }

    /// Whether a dense bitmap page still matches the checksum its parent leaf cell carries.
    ///
    /// This is the one parent-to-child integrity link in the tree, and it is a CRC over the
    /// whole 8 KiB page. A bit-sliced point read asks the question once per plane while reading
    /// one bit from each, so recomputing it per probe *was* the entire cost of reading an
    /// integer - 606ns of a 640ns plane, measured.
    ///
    /// A backend may therefore remember that a page number verified, and that is sound rather
    /// than a shortcut: under copy-on-write a page's bytes never change once written, because
    /// modifying one allocates a *new* page number. A remembered answer can only go stale when
    /// the number is recycled and written again, which is exactly where an implementation has
    /// to forget it.
    ///
    /// The default recomputes every time. Always correct, never wrong, just slow.
    fn verify_bitmap(&self, _pgno: Pgno, page: &Page, expected: u32) -> bool {
        big_page::bitmap_page_checksum(page) == expected
    }

    /// What this backend has done to the disk since it was opened, if it counts.
    ///
    /// **The backend counts, not the layer above.** A call into `read` is not an I/O, and how
    /// much of one it is differs per backend by more than a constant: a mapped read copies
    /// nothing and may not touch the disk at all, a file read is a syscall and 8 KiB, a
    /// key-value read is a lookup in somebody else's b-tree. Only the implementation can say,
    /// so only the implementation is asked.
    ///
    /// `None` means this backend does not keep the count - which is the honest answer for a
    /// pager with no disk under it - and a caller reporting metrics should omit the series
    /// rather than publish zeroes that look like an idle database.
    fn io_stats(&self) -> Option<IoStats> {
        None
    }
}

/// Write path. Split from `Pager` so a read-only replica can implement just the half it needs.
///
/// Takes `&self`, not `&mut self`: writer exclusivity is an invariant of `Store`, not of the
/// backend. With `&mut self` a reader and a writer could not coexist, and readers are meant
/// to keep running normally while a writer is active.
pub trait PagerMut: Pager {
    /// Writes one page. No fsync, and never through the mapping: going through the mapping
    /// would give up control over the order dirty pages reach the disk.
    fn write(&self, pgno: Pgno, page: &Page) -> Result<()>;

    /// Extends the file to `page_count` pages. Must run before touching any new page.
    fn grow(&self, page_count: u64) -> Result<()>;

    /// Shrinks the file to `page_count` pages. Callers must know nothing borrows past that
    /// point; a backend cannot check it.
    fn truncate(&self, page_count: u64) -> Result<()>;

    /// fsyncs data, and nothing else. The strongest flush the backend has.
    fn sync(&self) -> Result<()>;

    /// A weaker flush: to the operating system, not necessarily past the drive's write cache.
    ///
    /// Defaults to `sync`, so a backend with only one flush is correct without doing anything -
    /// promising more than asked for is never wrong. Only a backend that can actually tell the
    /// two apart overrides it.
    fn sync_data(&self) -> Result<()> {
        self.sync()
    }
}
