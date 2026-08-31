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

//! How hard a commit works before it reports success.
//!
//! The engine has always been at the maximum and had no way to say otherwise, which is the
//! honest default and stays the default. What it cost was a caller who genuinely can afford to
//! lose the last second - a bulk load that will be re-run from its source if the machine dies
//! mid-way - having to pay for a guarantee they did not need. This is the knob for that caller,
//! and nobody else.
//!
//! **The line every level respects.** A commit writes pages, flushes, writes the meta page,
//! flushes again. The first flush is not an optimisation: it is the entire reason there is no
//! WAL. If the meta page can reach the disk while the pages it names have not, a crash leaves a
//! meta pointing at bytes that were never written - and that is not lost data, it is a file
//! that does not open. So no level here skips only the first flush. The two move together, or
//! neither happens.
//!
//! What is being traded, then, is not atomicity but *how far up the stack* the last commits are
//! guaranteed to have travelled.

/// What a commit is willing to lose.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Durability {
    /// Flushes the drive's own write cache. A committed transaction survives power loss.
    ///
    /// The default, and what every existing file was written under.
    #[default]
    Full,
    /// Flushes to the operating system's idea of the disk, not past the drive's cache.
    ///
    /// On Linux this is `fdatasync`, which is the same call `Full` makes - the two differ only
    /// on macOS, where `Full` additionally issues `F_FULLFSYNC`. A committed transaction
    /// survives anything short of the drive losing its volatile cache.
    Barrier,
    /// Flushes nothing. The kernel writes the pages back when it feels like it.
    ///
    /// A committed transaction survives the *process* dying - the page cache outlives it and
    /// the file is coherent to anyone who opens it. It does not survive the machine going
    /// down: commits since the last writeback are gone, and because the two flushes were
    /// dropped together, what is left is an older consistent file rather than a broken one.
    None,
}

impl Durability {
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Barrier => "barrier",
            Self::None => "none",
        }
    }

    /// Parses the spelling used on a command line and in configuration.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "full" => Self::Full,
            "barrier" => Self::Barrier,
            "none" => Self::None,
            _ => return None,
        })
    }

    /// Whether `self` promises at least as much as `other`.
    ///
    /// Used to decide whether changing the setting has to flush first: relaxing costs nothing,
    /// but tightening has to make everything written under the looser setting durable, or the
    /// moment of the change would be a silent hole in the guarantee.
    pub fn at_least(self, other: Self) -> bool {
        self.rank() >= other.rank()
    }

    fn rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Barrier => 1,
            Self::Full => 2,
        }
    }

    pub(crate) fn as_u8(self) -> u8 {
        self.rank()
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::None,
            1 => Self::Barrier,
            _ => Self::Full,
        }
    }
}
