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

//! RESP2, which is five type markers and a length prefix.
//!
//! # Why this is written rather than linked
//!
//! The whole protocol this crate needs is `+ - : $ *`: a simple string, an error, an integer, a
//! length-prefixed blob, and an array of those. Commands go the other way as an array of blobs
//! and nothing else - there is no second encoding for arguments, no negotiation, and no state.
//! That is less code than the feature flags of a client library, and it keeps
//! `[dependencies]` holding one entry, which is the property `big-message` exists to have.
//!
//! RESP3 is not implemented and is not asked for: this never sends `HELLO`, so the server
//! answers in RESP2, which is what every version since 2.0 does by default.
//!
//! # Bytes, not strings
//!
//! A stream entry's field values are whatever the producer put there, and Redis does not promise
//! they are UTF-8. So a bulk string is `Vec<u8>` here and is only turned into text where a value
//! actually has to be one - see [`Value::text`], which refuses rather than replacing, because a
//! field silently becoming `U+FFFD` is a value written wrong rather than a message rejected.

use std::io::{BufRead, Write};

/// One RESP2 value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Value {
    /// `+OK`
    Simple(String),
    /// `-ERR unknown command`
    Error(String),
    /// `:42`
    Int(i64),
    /// `$5\r\nhello`
    Bulk(Vec<u8>),
    /// `$-1`, and `*-1`, which mean the same "there is nothing here" in the two places they
    /// appear. Folded into one variant because every caller here treats them alike.
    Nil,
    /// `*2\r\n...`
    Array(Vec<Value>),
}

/// What can go wrong reading or writing one.
#[derive(Debug)]
pub enum Error {
    /// The socket.
    Io(std::io::Error),
    /// Bytes that are not RESP2, or not the shape this crate expected.
    Protocol(String),
    /// The server answered `-ERR ...`, which is a refusal rather than a failure.
    ///
    /// Its own variant because the two need different handling: a refusal will be identical the
    /// second time, and a socket failure may not be.
    Server(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Protocol(m) => write!(f, "{m}"),
            Self::Server(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl Value {
    /// The text of a bulk or simple string.
    ///
    /// Refuses rather than replacing: a stream field that is not UTF-8 is a message this sink
    /// cannot write, and turning it into replacement characters would write a different value
    /// than the producer sent.
    pub fn text(&self) -> Result<&str, Error> {
        match self {
            Self::Bulk(b) => core::str::from_utf8(b)
                .map_err(|_| Error::Protocol("a value that is not UTF-8".to_string())),
            Self::Simple(s) => Ok(s),
            other => Err(Error::Protocol(format!("expected a string, got {other:?}"))),
        }
    }

    /// The elements of an array.
    pub fn array(&self) -> Result<&[Value], Error> {
        match self {
            Self::Array(items) => Ok(items),
            // A `Nil` array is how Redis says "the block expired with nothing to give you",
            // which is an ordinary answer rather than a fault, so it reads as no elements.
            Self::Nil => Ok(&[]),
            other => Err(Error::Protocol(format!("expected an array, got {other:?}"))),
        }
    }
}

/// Writes one command: an array of bulk strings, which is the only shape a command has.
pub fn write_command(out: &mut impl Write, args: &[&[u8]]) -> Result<(), Error> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        buf.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        buf.extend_from_slice(arg);
        buf.extend_from_slice(b"\r\n");
    }
    // One write for the whole command: a command split across writes is a command a server may
    // start parsing before it is complete, which costs a round trip for nothing.
    out.write_all(&buf)?;
    out.flush()?;
    Ok(())
}

/// Reads one value.
pub fn read_value(input: &mut impl BufRead) -> Result<Value, Error> {
    let line = read_line(input)?;
    let (marker, rest) = line.split_at(1);
    match marker {
        "+" => Ok(Value::Simple(rest.to_string())),
        "-" => Ok(Value::Error(rest.to_string())),
        ":" => rest
            .parse()
            .map(Value::Int)
            .map_err(|_| Error::Protocol(format!("not an integer: {rest:?}"))),
        "$" => {
            let len: i64 =
                rest.parse().map_err(|_| Error::Protocol(format!("not a length: {rest:?}")))?;
            if len < 0 {
                return Ok(Value::Nil);
            }
            let mut body = vec![0u8; len as usize];
            input.read_exact(&mut body)?;
            let mut crlf = [0u8; 2];
            input.read_exact(&mut crlf)?;
            if &crlf != b"\r\n" {
                return Err(Error::Protocol("a bulk string not ended by CRLF".to_string()));
            }
            Ok(Value::Bulk(body))
        }
        "*" => {
            let len: i64 =
                rest.parse().map_err(|_| Error::Protocol(format!("not a count: {rest:?}")))?;
            if len < 0 {
                return Ok(Value::Nil);
            }
            let mut items = Vec::with_capacity(len.min(1024) as usize);
            for _ in 0..len {
                items.push(read_value(input)?);
            }
            Ok(Value::Array(items))
        }
        other => Err(Error::Protocol(format!("not a RESP marker: {other:?}"))),
    }
}

/// One value, with `-ERR ...` turned into the refusal it is.
///
/// Separated from [`read_value`] because one caller wants the error as a value: `XGROUP CREATE`
/// on a group that exists answers `-BUSYGROUP`, which is the sink's normal start-up path rather
/// than a fault.
pub fn read_reply(input: &mut impl BufRead) -> Result<Value, Error> {
    match read_value(input)? {
        Value::Error(message) => Err(Error::Server(message)),
        other => Ok(other),
    }
}

fn read_line(input: &mut impl BufRead) -> Result<String, Error> {
    let mut line = Vec::new();
    input.read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Err(Error::Protocol("the server closed the connection".to_string()));
    }
    while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| Error::Protocol("a header that is not UTF-8".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &str) -> Result<Value, Error> {
        read_value(&mut std::io::BufReader::new(bytes.as_bytes()))
    }

    #[test]
    fn a_command_is_an_array_of_bulk_strings_and_nothing_else() {
        let mut out = Vec::new();
        write_command(&mut out, &[b"XACK", b"events", b"g1", b"1700-0"]).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "*4\r\n$4\r\nXACK\r\n$6\r\nevents\r\n$2\r\ng1\r\n$6\r\n1700-0\r\n"
        );
    }

    #[test]
    fn an_argument_holding_crlf_cannot_forge_a_second_command() {
        // The reason a command is length-prefixed rather than quoted: this argument contains
        // the separator, and it is still one argument.
        let mut out = Vec::new();
        write_command(&mut out, &[b"XACK", b"a\r\nDEL\r\nb"]).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "*2\r\n$4\r\nXACK\r\n$9\r\na\r\nDEL\r\nb\r\n");
    }

    #[test]
    fn every_marker_reads_as_what_it_marks() {
        assert_eq!(read("+OK\r\n").unwrap(), Value::Simple("OK".to_string()));
        assert_eq!(read(":42\r\n").unwrap(), Value::Int(42));
        assert_eq!(read("$5\r\nhello\r\n").unwrap(), Value::Bulk(b"hello".to_vec()));
        assert_eq!(read("$0\r\n\r\n").unwrap(), Value::Bulk(Vec::new()));
        assert_eq!(read("$-1\r\n").unwrap(), Value::Nil);
        assert_eq!(read("*-1\r\n").unwrap(), Value::Nil);
        assert_eq!(read("*0\r\n").unwrap(), Value::Array(Vec::new()));
        assert_eq!(read("-ERR nope\r\n").unwrap(), Value::Error("ERR nope".to_string()));
    }

    #[test]
    fn a_bulk_string_holding_crlf_is_read_by_its_length() {
        // Length-prefixed, so the separator inside it is data. Reading to the next CRLF would
        // cut this in half and leave the rest to be read as a command.
        assert_eq!(read("$4\r\na\r\nb\r\n").unwrap(), Value::Bulk(b"a\r\nb".to_vec()));
    }

    #[test]
    fn a_nested_array_is_the_shape_xreadgroup_answers_in() {
        // `[[stream, [[id, [field, value]]]]]`, which is the one shape this crate has to walk.
        let frame =
            "*1\r\n*2\r\n$6\r\nevents\r\n*1\r\n*2\r\n$6\r\n1700-0\r\n*2\r\n$1\r\na\r\n$1\r\nb\r\n";
        let value = read(frame).unwrap();
        let streams = value.array().unwrap();
        assert_eq!(streams.len(), 1);
        let pair = streams[0].array().unwrap();
        assert_eq!(pair[0].text().unwrap(), "events");
        let entries = pair[1].array().unwrap();
        let entry = entries[0].array().unwrap();
        assert_eq!(entry[0].text().unwrap(), "1700-0");
        let fields = entry[1].array().unwrap();
        assert_eq!(fields[0].text().unwrap(), "a");
        assert_eq!(fields[1].text().unwrap(), "b");
    }

    #[test]
    fn a_refusal_is_told_apart_from_a_value() {
        let mut input = std::io::BufReader::new(&b"-BUSYGROUP already exists\r\n"[..]);
        match read_reply(&mut input) {
            Err(Error::Server(m)) => assert_eq!(m, "BUSYGROUP already exists"),
            other => panic!("expected a server refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_value_that_is_not_utf8_is_refused_rather_than_replaced() {
        let value = Value::Bulk(vec![0xff, 0xfe]);
        assert!(value.text().is_err(), "a lossy conversion would write the wrong value");
    }

    #[test]
    fn a_connection_that_closes_mid_answer_is_an_error_and_not_an_empty_one() {
        assert!(read("").is_err());
        assert!(read("$10\r\nshort\r\n").is_err());
    }
}
