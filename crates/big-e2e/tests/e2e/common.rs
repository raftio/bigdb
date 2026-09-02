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

//! Finding the binaries, starting one, and stopping it however the test ends.
//!
//! **The daemon is a child process, so the interesting part is the failure path.** A test that
//! asserts and unwinds must not leave a `big serve` holding a port and a file lock, and a test
//! waiting for something that will never happen must fail rather than hang. Both are handled
//! here once: `Daemon` kills and reaps on `Drop`, and every wait has a deadline that fails with
//! a sentence rather than expiring silently.

#![allow(dead_code)] // Each module uses its own subset.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Once;
use std::time::{Duration, Instant};

/// How long anything here waits before calling it a failure.
///
/// Generous, because a cold `big serve` on a loaded machine can take a moment, and short enough that
/// a test that will never pass says so rather than holding the suite.
const PATIENCE: Duration = Duration::from_secs(20);

/// Which package builds which binary.
///
/// Two, from one package, where there were four from four. `big-bin` is the only crate in the
/// workspace with a `[[bin]]`, so this table is short by construction now rather than by
/// upkeep.
const BINARIES: &[(&str, &str)] = &[("big", "big-bin"), ("bigctl", "big-bin")];

/// The directory this test executable was built into, which is where the binaries land.
///
/// `current_exe` is `target/<profile>/deps/e2e-<hash>`, so the binaries are two levels up.
fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("a test executable has a path");
    exe.parent().and_then(Path::parent).expect("target/<profile>/deps/<exe>").to_path_buf()
}

/// The path to one shipped binary, building it first if it is not there yet.
///
/// **Built here rather than assumed.** `cargo test -p big-e2e` builds this crate and nothing
/// else, so under that invocation none of the four exists. Doing it in the test means the
/// suite runs the same way however it was started, instead of passing under `--workspace` and
/// failing on its own - which is the kind of difference nobody debugs twice.
pub fn bin(name: &str) -> PathBuf {
    static BUILT: Once = Once::new();
    let path = target_dir().join(name);
    if !path.exists() {
        BUILT.call_once(build_them_all);
    }
    assert!(path.exists(), "{} was not built; is it still a [[bin]]?", path.display());
    path
}

/// One `cargo build` for all four, so a suite that needs three does not pay for three builds.
fn build_them_all() {
    let profile = target_dir().file_name().and_then(|s| s.to_str()).unwrap_or("debug").to_string();
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    cmd.arg("build");
    for (binary, package) in BINARIES {
        cmd.args(["-p", package, "--bin", binary]);
    }
    if profile == "release" {
        cmd.arg("--release");
    }
    let status = cmd.status().expect("cargo is on PATH under `cargo test`");
    assert!(status.success(), "could not build the binaries under test");
}

/// What a run produced: the code a shell would see, and the two streams a user would.
pub struct Run {
    pub code: i32,
    pub out: String,
    pub err: String,
}

impl Run {
    /// Asserts the exit code, printing both streams when it is not the one expected.
    ///
    /// The streams are in the panic because a bare "left != right" on two small integers is the
    /// least useful thing a failing process test can say.
    pub fn expect(self, code: i32) -> Self {
        assert_eq!(
            self.code, code,
            "expected exit {code}, got {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.out, self.err
        );
        self
    }

    /// Whether either stream mentions this, since a tool may report on either.
    pub fn said(&self, what: &str) -> bool {
        self.out.contains(what) || self.err.contains(what)
    }
}

/// Runs one binary to completion.
pub fn run(name: &str, args: &[&str]) -> Run {
    run_with_stdin(name, args, "")
}

/// The same, with something on standard input - which is what `-` reads.
pub fn run_with_stdin(name: &str, args: &[&str], stdin: &str) -> Run {
    let mut child = Command::new(bin(name))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("could not run {name}: {e}"));
    child.stdin.take().expect("piped").write_all(stdin.as_bytes()).expect("the child reads it");
    let out = child.wait_with_output().expect("the child finishes");
    Run {
        // A process killed by a signal has no code. `-1` is not one any of these returns, so it
        // cannot be mistaken for an ordinary exit.
        code: out.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&out.stdout).into_owned(),
        err: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A port nothing is listening on, by asking the kernel for one and letting it go.
///
/// Racy in principle: something else can take it between the release and the bind. `Daemon` is
/// what makes that survivable - it notices a daemon that died and tries again with a new port,
/// rather than every test carrying the retry.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").expect("a loopback port").local_addr().expect("bound").port()
}

/// A directory that outlives the daemons working in it.
///
/// Separate from [`Daemon`] because a restart is two daemons and one file. When the daemon owned
/// the directory, stopping it deleted the database it was meant to leave behind - which is the
/// bug this type exists to make unrepresentable.
pub struct Workspace(tempfile::TempDir);

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    /// A fresh directory, removed when this value is dropped.
    pub fn new() -> Self {
        Self(tempfile::tempdir().expect("a temporary directory"))
    }

    /// A path inside it.
    pub fn join(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }

    /// The directory itself, for the few things that want it.
    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// A daemon on `data.big` inside this workspace, which the workspace outlives.
    pub fn daemon(&self, flags: &[&str]) -> Daemon {
        self.daemon_on("data.big", flags)
    }

    /// The same, on a named file.
    pub fn daemon_on(&self, file: &str, flags: &[&str]) -> Daemon {
        Daemon::spawn(self.join(file), self.join(&format!("{file}.log")), flags, 5)
    }

    /// A daemon on a named file at an address the caller chose.
    ///
    /// For a cluster, where the addresses are in a file both nodes read and so cannot be picked
    /// by the daemons. No retry on a busy port: the address is already written down.
    pub fn daemon_at(&self, file: &str, addr: &str, flags: &[&str]) -> Daemon {
        Daemon::at(self.join(file), self.join(&format!("{file}.log")), addr, flags)
    }
}

/// A running `big serve`, killed and reaped whenever the test ends.
///
/// Owns no directory. A test that only needs a daemon uses [`Daemon::start`], which keeps a
/// workspace alive alongside it; a test that outlives its daemon makes the [`Workspace`] first.
pub struct Daemon {
    child: Child,
    pub addr: SocketAddr,
    pub path: PathBuf,
    /// Where the daemon's own log went, so a test can read what it announced at startup.
    log: PathBuf,
    /// Only for [`Daemon::start`]: the workspace nobody else is holding.
    own: Option<Workspace>,
}

impl Daemon {
    /// A daemon on a fresh file in a workspace it keeps to itself.
    pub fn start() -> Self {
        Self::with(&[])
    }

    /// The same, with extra flags.
    pub fn with(flags: &[&str]) -> Self {
        let workspace = Workspace::new();
        let mut daemon = workspace.daemon(flags);
        daemon.own = Some(workspace);
        daemon
    }

    /// One try at starting, retrying on a port that turned out not to be free.
    ///
    /// A port handed out by `free_port` can be taken by something else before `big serve` binds it.
    /// That is nobody's fault and would otherwise be a flaky test in every module here, so it is
    /// handled once. `left` runs out rather than looping, because a `big serve` that cannot start for
    /// a *different* reason must fail rather than spin.
    fn spawn(path: PathBuf, log: PathBuf, flags: &[&str], left: u32) -> Self {
        let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().expect("loopback");
        let mut args = vec!["serve".to_string(), path.display().to_string(), addr.to_string()];
        args.extend(flags.iter().map(|f| (*f).to_string()));
        // Into a file rather than a pipe: a pipe nobody drains fills up and stops the daemon,
        // and this one has to keep running for as long as the test does.
        let sink = std::fs::File::create(&log).expect("a log file");
        let child = Command::new(bin("big"))
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::from(sink))
            .spawn()
            .expect("big serve starts");

        let mut daemon = Daemon { child, addr, path: path.clone(), log: log.clone(), own: None };
        if daemon.wait_until_healthy() {
            return daemon;
        }
        assert!(
            left > 1,
            "big serve never became healthy on any port tried; its log said:\n{}",
            daemon.log()
        );
        drop(daemon); // Killed and reaped here, before the port is asked for again.
        Self::spawn(path, log, flags, left - 1)
    }

    /// A daemon at an address the caller chose, which must be free.
    fn at(path: PathBuf, log: PathBuf, addr: &str, flags: &[&str]) -> Self {
        let parsed: SocketAddr = addr.parse().expect("host:port");
        let mut args = vec!["serve".to_string(), path.display().to_string(), addr.to_string()];
        args.extend(flags.iter().map(|f| (*f).to_string()));
        let sink = std::fs::File::create(&log).expect("a log file");
        let child = Command::new(bin("big"))
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::from(sink))
            .spawn()
            .expect("big serve starts");

        let mut daemon = Daemon { child, addr: parsed, path, log, own: None };
        assert!(
            daemon.wait_until_healthy(),
            "big serve never became healthy on {addr}; its log said:\n{}",
            daemon.log()
        );
        daemon
    }

    /// Polls `/health` until it answers, or the daemon dies, or patience runs out.
    fn wait_until_healthy(&mut self) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return false; // It exited. The caller decides whether that was the point.
            }
            if let Some((200, _)) = self.get("/health") {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Everything the daemon has said on stderr so far.
    ///
    /// Its startup lines are the only place several decisions are visible: which durability it
    /// is running under, how many tokens it loaded, which shards it owns.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// `--addr <this daemon>`, which every client call needs.
    pub fn addr_args(&self) -> [String; 2] {
        ["--addr".to_string(), self.addr.to_string()]
    }

    /// One `bigctl` subcommand against this daemon.
    ///
    /// One helper where there were two. A load used to be a different binary, so it needed its
    /// own; now `import` is a subcommand like `schema` is, and a test that loads a file says so
    /// in its arguments rather than in which method it called.
    pub fn bigctl(&self, args: &[&str]) -> Run {
        self.bigctl_stdin(args, "")
    }

    /// The same, with something on standard input.
    pub fn bigctl_stdin(&self, args: &[&str], stdin: &str) -> Run {
        let [flag, addr] = self.addr_args();
        let mut all = vec![flag.as_str(), addr.as_str()];
        all.extend_from_slice(args);
        run_with_stdin("bigctl", &all, stdin)
    }

    /// A bare `GET`, for the two routes that are never authenticated.
    fn get(&self, target: &str) -> Option<(u16, String)> {
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let request =
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
        stream.write_all(request.as_bytes()).ok()?;
        stream.flush().ok()?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw).ok()?;
        let status: u16 = raw.split(' ').nth(1)?.parse().ok()?;
        Some((status, raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string()))
    }

    /// Stops the daemon and waits for it to be gone.
    ///
    /// Waited for rather than signalled: the exclusive lock is held until the process is
    /// *reaped*, so anything that opens the file next - another daemon, or `big` - has to be
    /// after this and not merely after the kill.
    pub fn stop(mut self) {
        self.kill_and_reap();
    }

    fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // However the test ended, including a panic half way through an assertion.
        self.kill_and_reap();
    }
}

/// A port nothing is listening on, for a caller that has to write it down before binding it.
///
/// The same race `free_port` has, and here it cannot be retried away - a cluster file names the
/// address before either daemon starts. Kept separate so that the name says which one it is.
pub fn reserved_port() -> u16 {
    free_port()
}

/// A token file with the mode the server insists on.
pub fn token_file(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("tokens");
    std::fs::write(&path, body).expect("a token file");
    set_mode(&path, 0o600);
    path
}

/// Sets a file's permission bits, which is half of what the token file tests are about.
pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Waits for something to become true, failing with a sentence rather than hanging.
pub fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("waited {PATIENCE:?} for {what} and it never happened");
}
