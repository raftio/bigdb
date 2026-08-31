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

//! What a storage engine is, and the list of the ones this build has.
//!
//! An engine is described here and implemented in a module of its own - [`crate::bitmap`],
//! [`crate::columnar`], [`crate::hybrid`]. This file holds the part every other crate needs and
//! none of the part only the engine itself needs: the byte the catalog stores, the name every
//! outside surface reads and writes, and the two capability questions the write and scan paths
//! branch on.
//!
//! # Adding an engine
//!
//! 1. A module under `big-engine` holding the storage code, with a unit struct implementing
//!    [`Engine`].
//! 2. One line in [`ENGINES`].
//!
//! Nothing else derives from the choice. [`TableEngine::from_u8`], [`TableEngine::parse`],
//! [`TableEngine::all`] and every error message that lists the engines read [`ENGINES`] rather
//! than repeating it, so an engine that is in the list is in all of them and an engine that is
//! not is in none.
//!
//! What the list cannot do for a genuinely new *format* is write it: [`Engine::has_bitmap`] and
//! [`Engine::has_columns`] describe the two kinds of tree this build knows how to maintain, and
//! an engine storing something that is neither still needs code in the write path. The list
//! buys the identity, the naming and the routing; it does not buy the storage.

use core::fmt;

/// One storage engine, as everything outside it needs to know it.
///
/// Deliberately narrow. An engine does not get to describe its own write path here, because the
/// write path is not per-engine code - it is one path that asks these two capability questions.
/// A trait wide enough to hold `write` would be a trait with one implementor per combination of
/// answers, which is what [`ENGINES`] already is.
pub trait Engine: Send + Sync + 'static {
    /// The byte the catalog stores for a table under this engine.
    ///
    /// Part of the file format. Codes are never reused and never renumbered: a file written by
    /// an older build has to keep meaning what it meant.
    fn code(&self) -> u8;

    /// The name this engine is written and read as at every surface outside the engine - the
    /// SQL `ENGINE` clause, the HTTP schema route, the cluster wire's DDL.
    fn name(&self) -> &'static str;

    /// Whether this engine maintains bitmap and bit-sliced fragments for declared fields.
    fn has_bitmap(&self) -> bool;

    /// Whether this engine maintains column segments.
    fn has_columns(&self) -> bool;
}

/// Every engine this build knows, in code order.
///
/// The one place the set is written down. Adding an engine is adding a line here; see the
/// module docs.
pub static ENGINES: &[&'static dyn Engine] =
    &[&crate::bitmap::BitmapEngine, &crate::hybrid::HybridEngine, &crate::columnar::ColumnarEngine];

/// A table's engine: a handle into [`ENGINES`], the width of a pointer and `Copy`.
///
/// Not an enum, and that is the point. An enum would carry a second copy of the code, the name
/// and the capabilities alongside the [`Engine`] implementation that already has them, and two
/// copies of a mapping is one that can drift. Callers still write [`TableEngine::Bitmap`]; the
/// constant just resolves through the registry now.
#[derive(Clone, Copy)]
pub struct TableEngine(&'static dyn Engine);

// The constants keep the spelling every call site already used when this was an enum. Renaming
// them to SCREAMING_CASE would be a churn of the whole workspace to satisfy a lint about a
// thing that reads as a variant everywhere it appears.
#[allow(non_upper_case_globals)]
impl TableEngine {
    /// Bitmaps and bit-sliced indexes, and nothing else. What every table was before there was
    /// a choice.
    pub const Bitmap: Self = Self(&crate::bitmap::BitmapEngine);

    /// Both. The index answers what an index is good at, the columns answer what a scan is good
    /// at, and the planner picks. Costs a second copy of every fact.
    pub const BitmapColumnar: Self = Self(&crate::hybrid::HybridEngine);

    /// Column segments, plus the existence row and nothing else.
    ///
    /// The existence row stays even here, and deliberately: it is one bit per record, and it is
    /// what `Not`, `count(*)` and the record cursor stand on. Dropping it would cost far more
    /// than the bit it saves.
    pub const Columnar: Self = Self(&crate::columnar::ColumnarEngine);
}

impl TableEngine {
    /// The engine behind the handle, for a caller that wants to ask it something this type does
    /// not forward.
    pub fn spec(self) -> &'static dyn Engine {
        self.0
    }

    /// The byte the catalog stores. Public because the number is not private: it is what a peer
    /// is told when a table is created across a cluster, so the mapping has one definition and
    /// both readers use it.
    pub fn code(self) -> u8 {
        self.0.code()
    }

    /// The inverse of [`TableEngine::code`], for a caller holding a byte off disk or off the
    /// wire. `None` means a build that does not have this engine, which is refused rather than
    /// guessed at.
    pub fn from_u8(v: u8) -> Option<Self> {
        ENGINES.iter().find(|e| e.code() == v).map(|e| Self(*e))
    }

    /// Whether this engine maintains bitmap and bit-sliced fragments for declared fields.
    pub fn has_bitmap(self) -> bool {
        self.0.has_bitmap()
    }

    /// Whether this engine maintains column segments.
    pub fn has_columns(self) -> bool {
        self.0.has_columns()
    }

    /// The name this engine is written and read as, at every surface outside the engine.
    pub fn as_str(self) -> &'static str {
        self.0.name()
    }

    /// The inverse of [`TableEngine::as_str`], for a caller holding text.
    pub fn parse(s: &str) -> Option<Self> {
        ENGINES.iter().find(|e| e.name() == s).map(|e| Self(*e))
    }

    /// Every engine this build knows, for a caller listing them - a `--help`, an error message,
    /// a test that has to cover all of them.
    pub fn all() -> impl Iterator<Item = Self> {
        ENGINES.iter().map(|e| Self(*e))
    }

    /// The engine names, comma-separated, for the one error message that has to list them.
    ///
    /// Built rather than written out, so an engine added to [`ENGINES`] appears here without
    /// anyone remembering to come and add it.
    pub fn names() -> String {
        let names: Vec<&str> = ENGINES.iter().map(|e| e.name()).collect();
        names.join(", ")
    }
}

impl Default for TableEngine {
    /// What a table gets when the caller does not choose.
    ///
    /// Not [`TableEngine::Bitmap`], which is what a *decoded* zero means. A caller who says
    /// nothing wants the engine that answers the widest range of questions well, and a file that
    /// says nothing predates the choice entirely - two different questions that happen to share
    /// a type.
    fn default() -> Self {
        Self::BitmapColumnar
    }
}

/// By code, which is the identity: two handles to the same engine are the same engine.
impl PartialEq for TableEngine {
    fn eq(&self, other: &Self) -> bool {
        self.code() == other.code()
    }
}

impl Eq for TableEngine {}

impl core::hash::Hash for TableEngine {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.code().hash(state);
    }
}

/// The name, not the pointer. A test that fails on an engine mismatch should say which engine.
impl fmt::Debug for TableEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for TableEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codes and the names are the file format and the wire format. Pinned literally here,
    /// against the values rather than against each other, because a reorder of [`ENGINES`] or a
    /// tidy-up of a name would otherwise change what an existing file means with nothing
    /// failing. Zero in particular has to stay the bitmap engine: every file written before the
    /// engine byte existed carries a zero there, and those files are bitmap-only.
    #[test]
    fn codes_and_names_are_the_format() {
        for (engine, code, name) in [
            (TableEngine::Bitmap, 0u8, "bitmap"),
            (TableEngine::BitmapColumnar, 1, "bitmap+columnar"),
            (TableEngine::Columnar, 2, "columnar"),
        ] {
            assert_eq!(engine.code(), code);
            assert_eq!(engine.as_str(), name);
            assert_eq!(TableEngine::from_u8(code), Some(engine));
            assert_eq!(TableEngine::parse(name), Some(engine));
        }
    }

    /// A byte from a newer build is refused, not guessed at.
    #[test]
    fn an_unknown_code_is_none() {
        assert_eq!(TableEngine::from_u8(u8::MAX), None);
        assert_eq!(TableEngine::parse("bitmaps"), None);
    }

    /// The registry is the identity map: no two engines share a code or a name, and every one
    /// of them round-trips through both of its encodings. A new engine added with a code or a
    /// name already taken would silently shadow the old one on the way back in.
    #[test]
    fn registry_is_unique_and_round_trips() {
        let mut codes: Vec<u8> = ENGINES.iter().map(|e| e.code()).collect();
        let n = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), n, "two engines share a code");

        let mut names: Vec<&str> = ENGINES.iter().map(|e| e.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "two engines share a name");

        for e in TableEngine::all() {
            assert_eq!(TableEngine::from_u8(e.code()), Some(e));
            assert_eq!(TableEngine::parse(e.as_str()), Some(e));
        }
    }

    /// Every engine stores something. One that answered `false` to both would be a table that
    /// takes writes and holds nothing.
    #[test]
    fn every_engine_stores_something() {
        for e in TableEngine::all() {
            assert!(e.has_bitmap() || e.has_columns(), "{e:?} stores neither");
        }
    }
}
