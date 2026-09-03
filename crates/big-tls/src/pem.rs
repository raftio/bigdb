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

//! PEM in, DER out.
//!
//! Thirty lines over [`crate::base64`], rather than a crate. What a PEM parser has to get right
//! is not the encoding, it is the refusals - and the refusals here are about telling an operator
//! which of their three files they have swapped, which no general parser can do because no
//! general parser knows the file was supposed to be a key.

use crate::base64;
use std::io;
use std::path::Path;

/// One `-----BEGIN x-----` block: its label and the bytes between the dashes.
pub struct Block {
    /// What the `BEGIN` line called it: `CERTIFICATE`, `PRIVATE KEY`, and so on. Kept as written
    /// rather than parsed into an enum, because the useful thing to do with an unrecognised
    /// label is put it in the error message.
    pub label: String,
    /// The bytes between the dashes, decoded.
    pub der: Vec<u8>,
}

/// Redacted, because one of the labels this can hold is `PRIVATE KEY`. A derived `Debug` would
/// put a key into whatever printed it - a test failure, an `{:?}` somebody left in - and the
/// length is the only part of it that is ever useful to look at.
impl core::fmt::Debug for Block {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Block({}, {} bytes)", self.label, self.der.len())
    }
}

/// Every block in a PEM file, in order.
///
/// Anything outside a `BEGIN`/`END` pair is skipped rather than refused: an operator's note
/// above the certificate, and the human-readable dump `openssl x509 -text` leaves in front of
/// it, are both extremely common and neither is a mistake.
pub fn parse(text: &str) -> io::Result<Vec<Block>> {
    let mut blocks = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(label) = between(line.trim(), "-----BEGIN ", "-----") else { continue };
        let mut body = String::new();
        let mut closed = false;
        for line in lines.by_ref() {
            let line = line.trim();
            if let Some(end) = between(line, "-----END ", "-----") {
                if end != label {
                    return Err(bad(format!("a `{label}` block ends with `END {end}`")));
                }
                closed = true;
                break;
            }
            body.push_str(line);
        }
        if !closed {
            return Err(bad(format!("a `{label}` block has no `-----END {label}-----`")));
        }
        // Generous about the length here, and only here: a certificate chain block runs to a few
        // kilobytes and the ceiling that matters is on a header from a stranger, not on a file
        // the operator put on this disk themselves.
        let der = base64::decode(&body, 1 << 20)
            .map_err(|e| bad(format!("the body of a `{label}` block is {e}")))?;
        blocks.push(Block { label: label.to_string(), der });
    }
    Ok(blocks)
}

/// Reads a file and parses it, saying which file when it cannot.
///
/// Read as bytes rather than as a string so that a file which is *not* text gets the answer the
/// operator needs. A DER certificate handed to a flag that wanted PEM is the single most likely
/// mistake here, and "stream did not contain valid UTF-8" is not a sentence that helps anybody
/// find it.
pub fn parse_file(path: &Path) -> io::Result<Vec<Block>> {
    let bytes = std::fs::read(path)
        .map_err(|e| io::Error::new(e.kind(), format!("could not read {}: {e}", path.display())))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| bad(format!("{}: not text; is this a DER file?", path.display())))?;
    let blocks =
        parse(text).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    if blocks.is_empty() {
        return Err(bad(format!("{}: no `-----BEGIN`; this is not a PEM file", path.display())));
    }
    Ok(blocks)
}

fn between<'a>(line: &'a str, open: &str, close: &str) -> Option<&'a str> {
    line.strip_prefix(open)?.strip_suffix(close)
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CERT: &str = "\
-----BEGIN CERTIFICATE-----
Zm9vYmFy
-----END CERTIFICATE-----
";

    #[test]
    fn one_block() {
        let blocks = parse(CERT).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].label, "CERTIFICATE");
        assert_eq!(blocks[0].der, b"foobar");
    }

    #[test]
    fn a_chain_keeps_its_order() {
        let text = format!("{CERT}{}", CERT.replace("Zm9vYmFy", "Zm9v"));
        let blocks = parse(&text).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].der, b"foobar", "the leaf comes first and has to stay first");
        assert_eq!(blocks[1].der, b"foo");
    }

    #[test]
    fn preamble_and_notes_are_skipped() {
        // What `openssl x509 -text` writes above the block, and what an operator writes above
        // that. Refusing either would refuse most real certificate files.
        let text = format!("# issued 2026-03 for node a\nSubject: CN=a\n\n{CERT}");
        assert_eq!(parse(&text).unwrap().len(), 1);
    }

    #[test]
    fn a_mismatched_end_is_refused() {
        let text = "-----BEGIN CERTIFICATE-----\nZm9v\n-----END PRIVATE KEY-----\n";
        let e = parse(text).unwrap_err();
        assert!(e.to_string().contains("END PRIVATE KEY"), "{e}");
    }

    #[test]
    fn a_block_that_never_ends_is_refused() {
        // Truncation by a full disk or an interrupted copy. Silently returning nothing would
        // present as "no certificate configured", which sends the operator to the wrong file.
        let text = "-----BEGIN CERTIFICATE-----\nZm9v\n";
        let e = parse(text).unwrap_err();
        assert!(e.to_string().contains("no `-----END CERTIFICATE-----`"), "{e}");
    }

    #[test]
    fn a_body_that_is_not_base64_says_so() {
        let text = "-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n";
        let e = parse(text).unwrap_err();
        assert!(e.to_string().contains("CERTIFICATE"), "the message names the block: {e}");
    }

    #[test]
    fn a_der_file_handed_to_a_pem_flag_says_which_mistake_that_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cert.der");
        // The first bytes of any real DER certificate: a SEQUENCE tag and a two-byte length.
        std::fs::write(&path, [0x30u8, 0x82, 0x01, 0xff]).unwrap();
        let e = parse_file(&path).unwrap_err();
        assert!(e.to_string().contains("is this a DER file?"), "{e}");
    }

    #[test]
    fn a_text_file_with_no_blocks_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "the certificate is in the other file\n").unwrap();
        let e = parse_file(&path).unwrap_err();
        assert!(e.to_string().contains("not a PEM file"), "{e}");
    }
}
