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

//! Whole lines, two ceilings, and an offset.
//!
//! The one thing this file must never do is cut a line in half. A half line is not a malformed
//! request the server refuses - `field record` with the value missing is refused, but
//! `country 41 G` and `B` on the next chunk are two lines the server accepts, and the fact that
//! lands is wrong. So the split is at `\n` and nowhere else, and a line that cannot fit is an
//! error rather than something to divide.
//!
//! It also counts bytes rather than leaving the caller to measure the body it produced. The
//! number a checkpoint records is an offset **into the input**, and a resumed run starts partway
//! through one - so the count has to begin where the reader began, which only this end knows.

use std::io::BufRead;

/// One request's worth of input.
#[derive(Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The body, exactly as it will be sent: whole lines, each keeping its newline.
    pub body: String,
    /// How many lines are in it. Reported, not used - the server counts facts, and a blank line
    /// is a line here and nothing there.
    pub lines: usize,
    /// The input's byte offset just past this chunk's last line, and the only number a
    /// checkpoint needs: everything before it has been sent.
    pub end: u64,
}

/// What can go wrong before a chunk exists.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// A single line is longer than a whole request, so no way of chunking makes it fit. Named
    /// with its offset rather than only its line number, because a resumed run's first line is
    /// not the file's first line and an operator has to be able to find it.
    LineTooLong { at: u64, len: usize, cap: usize },
    /// The input could not be read, or was not UTF-8. The server refuses a body that is not, so
    /// discovering it here saves sending it.
    Read(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LineTooLong { at, len, cap } => write!(
                f,
                "the line at byte {at} is {len} bytes, past the {cap} a request may carry; \
                 raise --chunk-bytes, or the line is not one line"
            ),
            Self::Read(why) => write!(f, "could not read the input: {why}"),
        }
    }
}

/// Cuts a reader into chunks of whole lines.
pub struct Chunker<R> {
    input: R,
    max_bytes: usize,
    max_lines: usize,
    offset: u64,
    /// A line that has been read but not placed: it did not fit the chunk being built, so it
    /// opens the next one. Without this the reader would have to be able to un-read, which a
    /// `BufRead` cannot promise across a `read_line`.
    held: Option<String>,
}

impl<R: BufRead> Chunker<R> {
    /// `start` is where `input` sits in the file it came from - zero for a fresh run, the
    /// checkpoint's offset for a resumed one.
    pub fn new(input: R, max_bytes: usize, max_lines: usize, start: u64) -> Self {
        Self {
            input,
            max_bytes: max_bytes.max(1),
            max_lines: max_lines.max(1),
            offset: start,
            held: None,
        }
    }

    /// The next chunk, or `None` at the end of the input.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>, Error> {
        let mut body = String::new();
        let mut lines = 0usize;

        loop {
            if self.held.is_none() {
                let mut line = String::new();
                let read =
                    self.input.read_line(&mut line).map_err(|e| Error::Read(e.to_string()))?;
                if read == 0 {
                    break;
                }
                if line.len() > self.max_bytes {
                    return Err(Error::LineTooLong {
                        at: self.offset + body.len() as u64,
                        len: line.len(),
                        cap: self.max_bytes,
                    });
                }
                self.held = Some(line);
            }

            // Safe: the branch above filled it, or it was already full.
            let line = self.held.as_ref().expect("a line is held");
            // The first line of a chunk always goes in - it has already been checked against
            // the ceiling, so `body.is_empty()` cannot loop forever here.
            if !body.is_empty()
                && (body.len() + line.len() > self.max_bytes || lines >= self.max_lines)
            {
                break;
            }
            body.push_str(&self.held.take().expect("a line is held"));
            lines += 1;
        }

        if body.is_empty() {
            return Ok(None);
        }
        self.offset += body.len() as u64;
        Ok(Some(Chunk { body, lines, end: self.offset }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(text: &str, max_bytes: usize, max_lines: usize) -> Result<Vec<Chunk>, Error> {
        let mut c = Chunker::new(text.as_bytes(), max_bytes, max_lines, 0);
        let mut out = Vec::new();
        while let Some(chunk) = c.next_chunk()? {
            out.push(chunk);
        }
        Ok(out)
    }

    #[test]
    fn an_empty_input_has_no_chunks() {
        assert_eq!(chunks("", 100, 100).unwrap(), Vec::new());
    }

    #[test]
    fn one_short_input_is_one_chunk_ending_at_its_length() {
        let got = chunks("a 1 x\nb 2 y\n", 100, 100).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "a 1 x\nb 2 y\n");
        assert_eq!(got[0].lines, 2);
        assert_eq!(got[0].end, 12);
    }

    /// The property the whole file exists for.
    #[test]
    fn a_chunk_never_ends_mid_line() {
        // Six bytes a line, a ceiling that lands inside the second one.
        let got = chunks("a 1 x\nb 2 y\nc 3 z\n", 9, 100).unwrap();
        assert_eq!(got.len(), 3);
        for chunk in &got {
            assert!(chunk.body.ends_with('\n'), "{:?} was cut mid-line", chunk.body);
            assert!(chunk.body.len() <= 9, "{:?} is past the ceiling", chunk.body);
        }
        assert_eq!(
            got.iter().map(|c| c.body.as_str()).collect::<String>(),
            "a 1 x\nb 2 y\nc 3 z\n"
        );
    }

    #[test]
    fn the_line_ceiling_splits_too() {
        let got = chunks("a 1 x\nb 2 y\nc 3 z\nd 4 w\n", 1_000_000, 2).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].lines, 2);
        assert_eq!(got[1].lines, 2);
        assert_eq!(got[0].end, 12);
        assert_eq!(got[1].end, 24);
    }

    /// Not "split it anyway": there is no split that produces two lines the server would read
    /// as the one that was written.
    #[test]
    fn a_line_past_the_ceiling_is_an_error_naming_where_it_is() {
        let err = chunks("a 1 x\nbbbbbbbbbbbbbbbbbbbb\n", 10, 100).unwrap_err();
        assert_eq!(Error::LineTooLong { at: 6, len: 21, cap: 10 }, err);
        assert!(err.to_string().contains("byte 6"), "{err}");
    }

    /// A file that does not end in a newline still has a last line, and the offset still names
    /// its end - otherwise a resumed run would send it a second time forever.
    #[test]
    fn a_last_line_without_a_newline_is_still_a_line() {
        let got = chunks("a 1 x\nb 2 y", 100, 100).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "a 1 x\nb 2 y");
        assert_eq!(got[0].end, 11);
    }

    /// The server skips a blank line; this does not, because dropping one would make `end` a
    /// number that does not match the file it counts.
    #[test]
    fn blank_lines_are_carried_and_counted() {
        let got = chunks("a 1 x\n\nb 2 y\n", 100, 100).unwrap();
        assert_eq!(got[0].body, "a 1 x\n\nb 2 y\n");
        assert_eq!(got[0].end, 13);
    }

    /// What a resumed run does: the offsets continue the file rather than restart at zero.
    #[test]
    fn a_start_offset_is_where_the_offsets_continue_from() {
        let mut c = Chunker::new("c 3 z\n".as_bytes(), 100, 100, 12);
        let chunk = c.next_chunk().unwrap().unwrap();
        assert_eq!(chunk.end, 18);
    }

    /// A ceiling below one line's length would otherwise loop: the line never fits, and a chunk
    /// with nothing in it is not progress.
    #[test]
    fn a_ceiling_smaller_than_any_line_stops_rather_than_spinning() {
        assert!(matches!(chunks("aaaa\n", 2, 100), Err(Error::LineTooLong { .. })));
    }
}
