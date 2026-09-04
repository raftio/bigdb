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

//! The daemon as a process: the built `big` binary, a real port, and a real signal.
//!
//! Everything else in this crate's tests drives the server in-process, which is right for the
//! client and wrong for the two things only a process has - a command line and a signal
//! disposition. What is asserted here could not be asserted any other way: that `SIGTERM`
//! ends the daemon with an exit code rather than with a signal, and that a flag typed on the
//! command line reaches the line the daemon prints about itself.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

/// One request, one status line. Enough to know whether anybody is listening.
fn status_of(addr: SocketAddr, target: &str) -> Option<u16> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").ok()?;
    let mut reply = String::new();
    s.read_to_string(&mut reply).ok()?;
    reply.split_whitespace().nth(1)?.parse().ok()
}

fn waiting(what: &str, budget: Duration, mut check: impl FnMut() -> bool) {
    let started = Instant::now();
    while started.elapsed() < budget {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("gave up waiting for {what} after {:?}", started.elapsed());
}

/// **`SIGTERM` is a request to stand down, and it is honoured.** A supervisor sends it first -
/// `docker stop`, a pod being evicted, `systemctl stop` - and a daemon that dies of it rather
/// than answering it leaves sockets open and an agreement that only finds out from silence.
/// The tell is the exit status: a process killed by a signal has none, and one that stood down
/// has zero.
#[test]
fn sigterm_stands_the_daemon_down_rather_than_killing_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.big");
    let log = std::fs::File::create(dir.path().join("stderr")).unwrap();
    let addr = free_port();

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_big"))
        .arg("serve")
        .arg(&db)
        .arg(addr.to_string())
        // On the command line so that the line it produces can be read back below.
        .arg("--balance")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .expect("the built daemon starts");

    waiting("the daemon to answer", Duration::from_secs(15), || {
        status_of(addr, "/health") == Some(200)
    });

    let told = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()
        .expect("`kill` is on the path");
    assert!(told.success(), "the signal was sent");

    let status = {
        let started = Instant::now();
        loop {
            if let Some(s) = daemon.try_wait().unwrap() {
                break s;
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "the daemon did not stand down within fifteen seconds of SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    };
    assert_eq!(
        status.code(),
        Some(0),
        "stood down with an exit code, not killed by the signal: {status}"
    );
    assert_eq!(status_of(addr, "/health"), None, "and the port is closed");

    // The flag reached the announcement. Alone it does nothing, and it says so rather than
    // letting an operator wait for a balancer with nothing to balance.
    let said = std::fs::read_to_string(dir.path().join("stderr")).unwrap();
    assert!(said.contains("big serving"), "{said}");
    assert!(said.contains("--balance without --cluster"), "{said}");
}
