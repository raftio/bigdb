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

//! The knobs, and the ceilings they have to stay under.
//!
//! Every default here is a number under a server-side limit, and the gap is the point: a client
//! that batches right up to a ceiling fails the moment the ceiling moves down, and one that
//! leaves a margin does not.

use core::time::Duration;

/// How many rows one statement carries.
///
/// `big_sql::MAX_INSERT_ROWS` is a million, and a statement past it is refused whole. This sits
/// under that, and it is a **backstop rather than the working limit**: for anything but the
/// narrowest rows [`DEFAULT_MAX_BYTES`] is reached first, and that is deliberate, because bytes
/// are what a statement costs and a count of rows is not.
///
/// It was 8,000 once, mirroring a server ceiling of 10,000. That cost real throughput and it is
/// worth writing down why: the server commits once per request, so a batch capped at eight
/// thousand rows made 250 commits over four million facts where five would do, and ran at about
/// half the speed. Measured, 3.36 s against 1.81 s. A ceiling on the wrong quantity is not free.
pub const DEFAULT_MAX_ROWS: usize = 900_000;

/// How many bytes of statement text one request carries.
///
/// `big_http::MAX_BODY` is 8 MiB, checked against `Content-Length` **before** the body is read,
/// so a batch past it is a `413` rather than a partial write. Seven leaves the same one-megabyte
/// margin `bigctl import`'s `DEFAULT_CHUNK_BYTES` leaves, for the same reason.
pub const DEFAULT_MAX_BYTES: usize = 7 << 20;

/// How long a partly-filled batch waits for the row that would fill it.
///
/// A file loader has no equivalent, because a file ends. A producer's stream does not, so
/// without this the last few messages of a quiet minute sit in memory until the next busy one.
pub const DEFAULT_LINGER: Duration = Duration::from_millis(200);

/// How long to wait on the socket, matching the server's own `read_timeout`/`write_timeout`.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times a request that provably never arrived is sent again.
///
/// The same three `bigctl import` uses. **It does not apply to a request that was written in
/// full** - see [`crate::Error::Unknown`], which is the whole difference between this crate's
/// retry rule and the loader's.
pub const DEFAULT_RETRIES: u32 = 3;

/// What a [`crate::Producer`] is allowed to do.
#[derive(Clone, PartialEq, Debug)]
pub struct Config {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub linger: Duration,
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
    pub retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_rows: DEFAULT_MAX_ROWS,
            max_bytes: DEFAULT_MAX_BYTES,
            linger: DEFAULT_LINGER,
            connect_timeout: DEFAULT_IO_TIMEOUT,
            io_timeout: DEFAULT_IO_TIMEOUT,
            retries: DEFAULT_RETRIES,
        }
    }
}
