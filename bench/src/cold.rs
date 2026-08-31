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

//! Making a read cold, and being honest about how cold it actually got.
//!
//! **A cold read has two caches in the way, not one.** The operating system's page cache holds
//! the file, and the engine holds its own - a mapping in the case of `big` and `lmdb`, a block
//! cache in the case of `redb` and `fjall`, a page cache of its own in the case of SQLite.
//! Dropping only the first measures an engine that still has everything it needs in its own
//! memory; dropping only the second measures a read that goes to the OS and stops there.
//!
//! So the harness does both, and the second half - reopening the engine - always works. The
//! first needs root, and when it is not available the report says which of the two it got rather
//! than printing a number under a heading it did not earn.

/// How cold the read actually was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Coldness {
    /// The engine was reopened and the OS page cache was dropped. A read goes to the device.
    Cold,
    /// The engine was reopened but the page cache survived, because dropping it needs root.
    /// A read goes to the OS and no further.
    Reopened,
    /// Nothing was dropped.
    Warm,
}

impl Coldness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Reopened => "reopened",
            Self::Warm => "warm",
        }
    }

    /// One line explaining what the label above is worth, for the report to print once.
    pub fn caveat(self) -> &'static str {
        match self {
            Self::Cold => "engine reopened and the OS page cache dropped: reads reach the device",
            Self::Reopened => {
                "engine reopened, but the OS page cache was kept - dropping it needs root, so \
                 these are not device reads"
            }
            Self::Warm => "nothing was dropped",
        }
    }
}

/// Drops the operating system's page cache, if this process is allowed to.
///
/// Deliberately reports failure rather than hiding it. A harness that silently carried on would
/// put warm numbers in a column headed cold, which is worse than having no column: the reader
/// has no way to tell, and a cold-read figure that is really a warm one understates every
/// engine that would have gone to disk.
#[cfg(target_os = "linux")]
pub fn drop_page_cache() -> bool {
    use std::io::Write;
    // `sync` first: a dirty page cannot be dropped, so without it the drop is partial in a way
    // that depends on how much the benchmark just wrote.
    let synced = std::process::Command::new("sync")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if synced.is_err() {
        return false;
    }
    match std::fs::OpenOptions::new().write(true).open("/proc/sys/vm/drop_caches") {
        Ok(mut f) => f.write_all(b"3\n").is_ok(),
        Err(_) => false,
    }
}

/// macOS has `purge`, which needs the same privilege and is the closest equivalent.
#[cfg(target_os = "macos")]
pub fn drop_page_cache() -> bool {
    // Output discarded on purpose. Without root `purge` writes a line of its own to stderr for
    // every call, and this is called once per measured row - sixty lines of the same complaint
    // through the middle of a report whose entire value is that it can be read. The harness
    // already says whether the cache was dropped, in the one place a reader will look.
    std::process::Command::new("purge")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn drop_page_cache() -> bool {
    false
}
