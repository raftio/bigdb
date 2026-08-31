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

//! Where a load got to, written down after the server said so.
//!
//! One number does the work - the input's byte offset past the last chunk the server
//! acknowledged - and it is exact rather than approximate for two reasons that have to hold
//! together: chunks are sent in file order, and a chunk resent is a chunk sent once. Take away
//! either and this becomes a guess.
//!
//! **Written after the acknowledgement, never before.** A process that dies between the two
//! resumes one chunk early and writes the same bits again, which costs a request. Writing first
//! would resume one chunk late and skip facts, which costs data and shows no symptom.
//!
//! The file also records what it is a checkpoint *of*, and refuses to resume when that has
//! changed. It records the input's size and **not a hash of its contents**: hashing 100 GB to
//! decide whether to skip reading 100 GB is not a saving, and a check that is affordable and
//! stated is better than one that is neither.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

/// A load, and where it got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// The route this load is sending to, `/table/tx/import`. A checkpoint from an `import`
    /// must not resume a `delete`.
    pub target: String,
    /// The input's path, as it was given.
    pub input: String,
    /// The input's size when the load started.
    pub size: u64,
    /// Bytes of the input the server has acknowledged.
    pub offset: u64,
    /// Lines in those bytes, and facts the server said it wrote. Carried so that a resumed run
    /// can report a total for the whole load rather than for its last leg.
    pub lines: u64,
    pub wrote: u64,
}

impl Checkpoint {
    /// Reads one, or `None` when there is no file - which is a fresh load, not a failure.
    pub fn read(path: &Path) -> Result<Option<Self>, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        Self::parse(&text).map(Some).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Strict on purpose. A key this build does not know is an error rather than something to
    /// skip, because the one thing a half-understood checkpoint can do is resume in the wrong
    /// place - and unlike a catalog record, there is no version of this file that a later build
    /// has to keep reading.
    fn parse(text: &str) -> Result<Self, String> {
        let (mut target, mut input) = (None, None);
        let (mut size, mut offset, mut lines, mut wrote) = (None, None, None, None);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) =
                line.split_once(' ').ok_or_else(|| format!("not `key value`: {line:?}"))?;
            let number = |v: &str| v.parse::<u64>().map_err(|_| format!("{key} is not a number"));
            match key {
                "target" => target = Some(value.to_string()),
                "input" => input = Some(value.to_string()),
                "size" => size = Some(number(value)?),
                "offset" => offset = Some(number(value)?),
                "lines" => lines = Some(number(value)?),
                "wrote" => wrote = Some(number(value)?),
                other => return Err(format!("unknown key `{other}`")),
            }
        }
        let missing = |what: &str| format!("no `{what}` in this checkpoint");
        Ok(Self {
            target: target.ok_or_else(|| missing("target"))?,
            input: input.ok_or_else(|| missing("input"))?,
            size: size.ok_or_else(|| missing("size"))?,
            offset: offset.ok_or_else(|| missing("offset"))?,
            lines: lines.unwrap_or(0),
            wrote: wrote.unwrap_or(0),
        })
    }

    /// Writes it whole, through a temporary file and a rename.
    ///
    /// The rename is the same trick `big compact` ends with, for the same reason: a checkpoint
    /// truncated by a crash mid-write would be read as a smaller offset - which resends work -
    /// or as no offset at all, which resends the load.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        let mut text = String::new();
        let _ = write!(
            text,
            "# bigi checkpoint. Delete this file to start the load over.\n\
             target {}\ninput {}\nsize {}\noffset {}\nlines {}\nwrote {}\n",
            self.target, self.input, self.size, self.offset, self.lines, self.wrote
        );

        let temp = path.with_extension("tmp");
        let mut file = std::fs::File::create(&temp)
            .map_err(|e| format!("could not write {}: {e}", temp.display()))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("could not write {}: {e}", temp.display()))?;
        std::fs::rename(&temp, path)
            .map_err(|e| format!("could not replace {}: {e}", path.display()))
    }

    /// Whether this checkpoint is about the load that is starting, and where it should resume.
    ///
    /// A mismatch is refused rather than ignored. The failure this prevents is the quiet one: a
    /// checkpoint left over from another file, applied to this one, skips its first N bytes and
    /// the load looks like it worked.
    pub fn resume_at(&self, target: &str, input: &str, size: u64) -> Result<u64, String> {
        if self.target != target {
            return Err(format!(
                "this checkpoint is for {}, and this load is {target}",
                self.target
            ));
        }
        if self.input != input {
            return Err(format!(
                "this checkpoint is for {}, and this load is reading {input}",
                self.input
            ));
        }
        if self.size != size {
            return Err(format!(
                "{input} was {} bytes when the load started and is {size} now; a checkpoint \
                 names an offset into the file it was taken from",
                self.size
            ));
        }
        if self.offset > size {
            return Err(format!("the checkpoint is past the end of {input}"));
        }
        Ok(self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Checkpoint {
        Checkpoint {
            target: "/table/tx/import".to_string(),
            input: "facts.txt".to_string(),
            size: 4096,
            offset: 1024,
            lines: 40,
            wrote: 40,
        }
    }

    #[test]
    fn what_is_written_is_what_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("load.ck");
        sample().write(&path).unwrap();
        assert_eq!(Checkpoint::read(&path).unwrap(), Some(sample()));
    }

    #[test]
    fn no_file_is_a_fresh_load_rather_than_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Checkpoint::read(&dir.path().join("absent")).unwrap(), None);
    }

    #[test]
    fn a_checkpoint_for_another_file_is_refused() {
        let err = sample().resume_at("/table/tx/import", "other.txt", 4096).unwrap_err();
        assert!(err.contains("facts.txt"), "{err}");
    }

    #[test]
    fn a_checkpoint_for_another_route_is_refused() {
        let err = sample().resume_at("/table/tx/delete", "facts.txt", 4096).unwrap_err();
        assert!(err.contains("/table/tx/import"), "{err}");
    }

    /// The file grew or shrank, so the offset names a place that is no longer there.
    #[test]
    fn a_resized_input_is_refused() {
        let err = sample().resume_at("/table/tx/import", "facts.txt", 9000).unwrap_err();
        assert!(err.contains("4096 bytes"), "{err}");
    }

    #[test]
    fn a_matching_checkpoint_gives_its_offset() {
        assert_eq!(sample().resume_at("/table/tx/import", "facts.txt", 4096), Ok(1024));
    }

    #[test]
    fn a_key_this_build_does_not_know_is_an_error() {
        let err = Checkpoint::parse("target /t\ninput f\nsize 1\noffset 0\nrate 9\n").unwrap_err();
        assert!(err.contains("rate"), "{err}");
    }
}
