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

//! The only `unsafe` module on the storage path. No crate above this one sees any `unsafe`.
//!
//! Safety argument. Four constraints, all of them enforced inside this file:
//!  1. `mapsize` is reserved once at open and never remapped, so no `&'a Page` goes dangling.
//!  2. `flock(LOCK_EX)` stops any other process truncating or overwriting under the mapping.
//!  3. Writes go through `pwrite`, never the mapping, keeping control over ordering.
//!  4. `read` is bounded by `file_pages`, not `mapsize`, so nothing past EOF is ever touched.
//!
//! What remains is SIGBUS on media failure. Accepted: copy-on-write makes every crash point
//! safe, so a bad disk costs availability, never correctness.

#![allow(unsafe_code)]

use crate::error::{Result, StoreError};
use crate::pager::{Pager, PagerMut};
use big_page::{meta::META_PAGES, Page, Pgno, PAGE_SIZE};
use memmap2::{Mmap, MmapOptions};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Flushes the drive's own write cache, not just the OS page cache.
///
/// On macOS `fsync(2)` returns once the data reaches the driver; the disk may still hold it in
/// a volatile cache. A power cut there reorders the two syncs a commit depends on, and the
/// whole atomicity argument goes with it. `F_FULLFSYNC` is the only call that actually waits.
/// Linux `fdatasync` already has the guarantee.
#[cfg(target_os = "macos")]
fn full_sync(file: &File) -> Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        // Some filesystems reject F_FULLFSYNC; fall back rather than fail the commit.
        file.sync_data()?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn full_sync(file: &File) -> Result<()> {
    file.sync_data()?;
    Ok(())
}

/// 1 TB of virtual address space, which is close to free on 64-bit.
pub const DEFAULT_MAPSIZE: u64 = 1 << 40;

/// Lock stripes over the verified-page memo. Point reads hit it once per bit plane from every
/// reader at once, so one lock would serialise exactly the path the memo exists to speed up.
const VERIFY_SHARDS: usize = 64;

/// Entries per stripe before the memo is emptied. A crude cap on purpose: the memo is an
/// optimisation, so forgetting all of it costs one CRC per page and never a wrong answer,
/// which is a far better trade than carrying an eviction policy for it.
const VERIFY_SHARD_CAP: usize = 4096;

pub struct MmapPager {
    file: File,
    map: Mmap,
    mapsize_pages: u64,
    /// Atomic because `grow` takes `&self`; writer exclusivity is enforced by `Store` above.
    file_pages: AtomicU64,
    /// Page numbers whose dense-bitmap checksum has already been verified, and the checksum
    /// they verified against. See [`Pager::verify_bitmap`] for why remembering is sound.
    verified: Vec<RwLock<std::collections::HashMap<Pgno, u32>>>,
    /// Counts what this backend does to the file. `read` below copies nothing, so what it
    /// records is how often the engine asked for a page rather than how many reached the disk -
    /// that last one is a page fault, and this process is not told about it. `write` is a real
    /// `pwrite`, so its bytes are real.
    io: crate::io::IoCounters,
}

impl MmapPager {
    pub fn open(path: impl AsRef<Path>, mapsize: u64) -> Result<Self> {
        // `truncate(false)` spelled out: opening an existing file must never erase it.
        let file =
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;

        // The exclusive lock must be taken BEFORE mapping, or another process has a window.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(StoreError::Locked);
        }

        let mapsize_pages = mapsize / PAGE_SIZE as u64;
        if mapsize_pages == 0 {
            return Err(StoreError::Unsupported("mapsize nhỏ hơn một page"));
        }

        // Map the full `mapsize` even though the file is shorter; `read` never touches past EOF.
        let map = unsafe { MmapOptions::new().len(mapsize as usize).map(&file)? };

        // **An empty path and somebody else's file are not the same thing.** A database is at
        // least two pages, so anything between empty and that is a file this engine did not
        // write - and `Store::open_or_init` would otherwise create a database on top of it,
        // because a file that short holds zero *pages* and so looks exactly like a new one.
        // Refused here rather than there because the length in bytes is only known here.
        let bytes = file.metadata()?.len();
        if bytes > 0 && bytes < META_PAGES * PAGE_SIZE as u64 {
            return Err(StoreError::NotADatabase { bytes });
        }

        let file_pages = bytes / PAGE_SIZE as u64;
        Ok(Self {
            file,
            map,
            mapsize_pages,
            file_pages: AtomicU64::new(file_pages),
            verified: (0..VERIFY_SHARDS).map(|_| RwLock::new(Default::default())).collect(),
            io: crate::io::IoCounters::new("mmap"),
        })
    }

    pub fn open_default(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path, DEFAULT_MAPSIZE)
    }
}

impl Pager for MmapPager {
    type Ref<'a> = &'a Page;

    fn read(&self, pgno: Pgno) -> Result<&Page> {
        let page_count = self.page_count();
        if pgno as u64 >= page_count {
            return Err(StoreError::OutOfBounds { pgno, page_count });
        }
        let off = pgno as usize * PAGE_SIZE;

        self.io.read(pgno);

        // SAFETY: off + PAGE_SIZE <= EOF, checked just above, so the region is backed by the
        // file. The mmap base is system-page aligned (>= 4096) and off is a multiple of 8192,
        // so the pointer is 8-aligned as `#[repr(align(8))] Page` requires. The mapping is
        // owned by `self` and never remapped, so the reference cannot outlive its backing.
        Ok(unsafe { &*(self.map.as_ptr().add(off) as *const Page) })
    }

    fn page_count(&self) -> u64 {
        self.file_pages.load(Ordering::Acquire)
    }

    fn capacity(&self) -> Option<u64> {
        Some(self.mapsize_pages)
    }

    fn io_stats(&self) -> Option<crate::io::IoStats> {
        Some(self.io.snapshot())
    }

    /// Verifies once per page number and remembers it. Sound because copy-on-write never
    /// rewrites a page in place; `write` is where the memory is given up again.
    fn verify_bitmap(&self, pgno: Pgno, page: &Page, expected: u32) -> bool {
        let shard = &self.verified[pgno as usize % VERIFY_SHARDS];
        if shard.read().unwrap().get(&pgno) == Some(&expected) {
            return true;
        }
        if big_page::bitmap_page_checksum(page) != expected {
            return false;
        }
        let mut w = shard.write().unwrap();
        if w.len() >= VERIFY_SHARD_CAP {
            w.clear();
        }
        w.insert(pgno, expected);
        true
    }
}

impl MmapPager {
    /// Forgets a page number's remembered verification. Must run on every path that changes
    /// what the number holds, or a recycled page could be trusted on its previous contents.
    fn forget_verified(&self, pgno: Pgno) {
        self.verified[pgno as usize % VERIFY_SHARDS].write().unwrap().remove(&pgno);
    }
}

impl PagerMut for MmapPager {
    fn write(&self, pgno: Pgno, page: &Page) -> Result<()> {
        let page_count = self.page_count();
        if pgno as u64 >= page_count {
            return Err(StoreError::OutOfBounds { pgno, page_count });
        }
        // Before the write, not after: a reader racing this write must never find a memo that
        // outlived the bytes it was about.
        self.forget_verified(pgno);
        self.file.write_all_at(page.as_bytes(), pgno as u64 * PAGE_SIZE as u64)?;
        self.io.write();
        Ok(())
    }

    fn grow(&self, page_count: u64) -> Result<()> {
        if page_count <= self.page_count() {
            return Ok(());
        }
        if page_count > self.mapsize_pages {
            return Err(StoreError::MapSizeExhausted {
                need: page_count,
                mapsize: self.mapsize_pages,
            });
        }
        self.file.set_len(page_count * PAGE_SIZE as u64)?;
        self.file_pages.store(page_count, Ordering::Release);
        self.io.grow();
        Ok(())
    }

    fn truncate(&self, page_count: u64) -> Result<()> {
        if page_count < self.page_count() {
            // Order matters: shorten the bound first, so no read can slip into the region
            // between the two calls and hit unbacked mapping.
            self.file_pages.store(page_count, Ordering::Release);
            self.file.set_len(page_count * PAGE_SIZE as u64)?;
            // `write` already covers correctness for a page that comes back; this only stops
            // the memo carrying entries for pages that no longer exist.
            for shard in &self.verified {
                shard.write().unwrap().retain(|&p, _| (p as u64) < page_count);
            }
            self.io.truncate();
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.io.sync(|| full_sync(&self.file))
    }

    /// `fdatasync`, which on Linux is exactly what `sync` does and on macOS is the half of it
    /// that does not wait for the drive.
    fn sync_data(&self) -> Result<()> {
        self.io.sync(|| self.file.sync_data())?;
        Ok(())
    }
}
