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

/// Parsing never panics: every bad input comes out as one of these variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageError {
    /// The page's `PageType` is not what the caller expected.
    TypeMismatch {
        expected: u8,
        found: u8,
    },
    UnknownPageType(u8),
    /// cell_count is so large that the cell index alone overruns the page.
    CellCountOverflow(u16),
    /// A cell offset falls outside the valid data area.
    CellOffsetOutOfRange {
        index: usize,
        offset: u16,
    },
    /// Offset is not a multiple of 8, so the payload cast would break.
    CellMisaligned {
        index: usize,
        offset: u16,
    },
    /// The cell index must be strictly increasing, otherwise two cells overlap.
    CellOrderBroken {
        index: usize,
    },
    /// The declared payload is longer than the space left on the page.
    PayloadOutOfRange {
        index: usize,
    },
    UnknownContainerType(u16),
    /// A cell holding column values was asked for a set. The type is known and the page is
    /// sound; the caller is reading a block of numbers as a list of bit offsets. Separate from
    /// `UnknownContainerType` because the two call for opposite actions - that one means the
    /// file is from a newer build, this one means a reader went to the wrong tree.
    NotAContainer(u16),
    /// `bytemuck` refused the cast; should be unreachable once offsets are aligned.
    Misaligned,
    ChecksumMismatch {
        stored: u32,
        computed: u32,
    },
    BadMagic(u32),
    UnsupportedVersion(u32),
    /// The meta page declares a page_size this build does not use.
    PageSizeMismatch {
        expected: u32,
        found: u32,
    },
}

/// Messages aimed at whoever has to act on them.
///
/// Most of these mean the same thing in practice - this page is not what it claims to be, and
/// something outside the engine damaged it - so they say so plainly and keep the numbers for
/// anyone reading further. `UnsupportedVersion` is the exception: it is the one variant with
/// a different remedy, so it names both versions and points at the tool.
impl core::fmt::Display for PageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TypeMismatch { expected, found } => {
                write!(f, "page is type {found}, expected {expected}")
            }
            Self::UnknownPageType(t) => write!(f, "page claims unknown type {t}"),
            Self::CellCountOverflow(n) => {
                write!(f, "page claims {n} cells, more than can fit")
            }
            Self::CellOffsetOutOfRange { index, offset } => {
                write!(f, "cell {index} points outside the page, at {offset}")
            }
            Self::CellMisaligned { index, offset } => {
                write!(f, "cell {index} is at {offset}, which is not 8-byte aligned")
            }
            Self::CellOrderBroken { index } => {
                write!(f, "cell {index} overlaps the one before it")
            }
            Self::PayloadOutOfRange { index } => {
                write!(f, "cell {index} claims more payload than the page holds")
            }
            Self::UnknownContainerType(t) => write!(f, "unknown container type {t}"),
            Self::NotAContainer(t) => {
                write!(f, "container type {t} holds column values, not a set of offsets")
            }
            Self::Misaligned => write!(f, "payload is misaligned"),
            Self::ChecksumMismatch { stored, computed } => write!(
                f,
                "checksum mismatch: page stores {stored:#010x}, its bytes compute {computed:#010x}"
            ),
            Self::BadMagic(m) => {
                write!(
                    f,
                    "not a big page: magic is {m:#010x}, expected {:#010x}",
                    crate::meta::MAGIC
                )
            }
            Self::UnsupportedVersion(v) => write!(
                f,
                "file format version {v}, but this build reads and writes version {}; \
                 a file from another version has to be dumped and reloaded, never migrated \
                 in place",
                crate::meta::VERSION
            ),
            Self::PageSizeMismatch { expected, found } => {
                write!(f, "file uses {found}-byte pages, this build uses {expected}")
            }
        }
    }
}

/// A stable, machine-readable name for the failure.
///
/// Separate from `Display` because the two have different audiences and different lifetimes:
/// the sentence is written for a person and may be reworded whenever it reads better, while
/// the code is what a script matches on and does not change without a version bump.
///
/// Most variants here collapse to `page_damaged` on purpose. They describe different ways a
/// page can be malformed, but every one of them calls for the same action - restore from a
/// backup - and a code exists to select an action, not to restate the message. The three that
/// keep their own code are the three with a different remedy.
impl PageError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ChecksumMismatch { .. } => "page_checksum_mismatch",
            Self::BadMagic(_) => "not_a_big_file",
            Self::UnsupportedVersion(_) => "unsupported_format_version",
            Self::PageSizeMismatch { .. } => "page_size_mismatch",
            Self::TypeMismatch { .. }
            | Self::UnknownPageType(_)
            | Self::CellCountOverflow(_)
            | Self::CellOffsetOutOfRange { .. }
            | Self::CellMisaligned { .. }
            | Self::CellOrderBroken { .. }
            | Self::PayloadOutOfRange { .. }
            | Self::UnknownContainerType(_)
            | Self::NotAContainer(_)
            | Self::Misaligned => "page_damaged",
        }
    }
}

impl core::error::Error for PageError {}
