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

//! Noticing that the client has gone, while the query is still running.
//!
//! A thread rather than a poll inside the scan, for the reason the type comment gives: the
//! thread running the query is the problem, so the thread that watches cannot be it.

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Watches a socket for the client hanging up, and flips a flag when it does.
///
/// **Why a thread rather than a poll inside the scan.** The thread running the query is busy;
/// that is the entire problem. Noticing an EOF means reading the socket, and the only thread
/// free to do that is one that is not running the query.
///
/// **Why it has to be joined.** `try_clone` duplicates the descriptor, and `O_NONBLOCK` lives
/// on the open file description that both copies share - so while the watchdog has the socket
/// non-blocking, so does the writer. Joining first, then restoring blocking mode, is what
/// keeps the response from meeting a spurious `WouldBlock` half way out.
pub(crate) struct Watchdog {
    handle: Option<std::thread::JoinHandle<()>>,
    /// Set to stop, with the condvar to wake the thread out of its wait immediately - a plain
    /// sleep loop would add its whole poll interval to the latency of every fast query.
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl Watchdog {
    pub(crate) fn spawn(stream: &TcpStream, cancel: Arc<AtomicBool>) -> Self {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let Ok(watched) = stream.try_clone() else {
            // Without a second handle there is nothing to watch with. A query that cannot be
            // cancelled is worse than one that can, and better than a refused request.
            return Self { handle: None, done };
        };
        if watched.set_nonblocking(true).is_err() {
            return Self { handle: None, done };
        }

        let signal = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            const POLL: Duration = Duration::from_millis(50);
            let (lock, cv) = &*signal;
            let mut stop = lock.lock().unwrap();
            while !*stop {
                let mut byte = [0u8; 1];
                match watched.peek(&mut byte) {
                    // Zero bytes from a peek is end of stream: the client hung up. This is the
                    // case the whole thread exists for.
                    Ok(0) => {
                        cancel.store(true, Ordering::Relaxed);
                        return;
                    }
                    // Bytes are waiting. The peer is still there, and this is a peek at the raw
                    // socket, so under TLS these are ciphertext - a pipelined request, or a key
                    // update. Ignoring them is right in every one of those cases: the question
                    // this thread asks is "has the client gone", and bytes arriving are the
                    // strongest possible answer of "no".
                    Ok(_) => {}
                    // Nothing has arrived yet, which is the normal case for a live socket.
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    // A signal interrupted the call. Nothing has been learned; look again.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    // Anything else - a reset, a broken pipe - means the connection is not
                    // going to deliver a response either way.
                    Err(_) => {
                        cancel.store(true, Ordering::Relaxed);
                        return;
                    }
                }
                let (guard, _) = cv.wait_timeout(stop, POLL).unwrap();
                stop = guard;
            }
        });
        Self { handle: Some(handle), done }
    }

    /// Stops the watchdog and puts the socket back into blocking mode.
    pub(crate) fn stop(self, stream: &TcpStream) {
        let (lock, cv) = &*self.done;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        if let Some(h) = self.handle {
            let _ = h.join();
        }
        // Only safe once the watchdog is joined; see the type comment.
        let _ = stream.set_nonblocking(false);
    }
}
