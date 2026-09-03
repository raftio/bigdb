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

//! Users, passwords and three roles.
//!
//! **This file used to argue against everything in it.** It said there was no subject to attach
//! a name to, and that hashing a token would be guarding the smaller of two secrets because
//! anyone who could read the token file could read the database file beside it. Both were true
//! of tokens. Neither is true of a password: a password is a thing a person also uses somewhere
//! else, so the file is no longer the smaller secret, and "who" is no longer nothing - it is the
//! person who chose it. That is what changed the answer, not the cost of an argon2 dependency.
//!
//! **The shape of a request's authorisation, once.** [`Auth::authorize`] runs one argon2
//! verification and returns a [`Principal`]; everything downstream compares against that
//! principal rather than asking again. Under the old design re-checking cost a constant-time
//! byte comparison and `POST /sql` did it twice without anybody minding. Under this one a second
//! check would cost a second fifty-millisecond hash on every statement, which is why `refuse`
//! hands the principal back instead of a yes.
//!
//! **A verification is expensive on purpose, so it is rationed three ways.** A cache keyed on a
//! secret nobody outside this process knows, so a busy client pays once a minute rather than
//! once a request. A ceiling on how many verifications run at once, because argon2's memory cost
//! is per verification and an unbounded number of wrong passwords is a memory exhaustion bought
//! with TCP connections. And nothing negative is ever cached, because caching a failure turns an
//! offline dictionary attack into an online one at the speed of a hash table.
//!
//! **What a node presents is not a password.** A peer proves itself with a client certificate
//! during the handshake, so [`Identity`] arrives already decided and short-circuits all of the
//! above - the fan-out between nodes never pays for a hash. See [`crate::routes`] for why that
//! makes a peer a different kind of caller rather than a very privileged person.

use big_embed::Authority;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub use big_tls::Identity;

/// What a credential is allowed to do. Ordered: each role contains the ones below it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Role {
    /// Read the schema and run queries.
    Read = 0,
    /// The above, plus writing and deleting records.
    Write = 1,
    /// The above, plus creating and dropping schema, and reading metrics.
    Admin = 2,
}

impl Role {
    /// The name used in the users file and in log lines. The same spelling in both, so a grep
    /// for a role in the log finds the line in the file that granted it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }

    /// Parses a role by the name the users file spells it with.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Self::Read,
            "write" => Self::Write,
            "admin" => Self::Admin,
            _ => return None,
        })
    }
}

/// What a statement demands, as what a credential has to hold.
///
/// **The whole of what this crate knows about SQL.** Which statements write and which only read
/// is [`big_embed::Sql::authority`]'s to say, next to the variants it is about; this is the one
/// line that turns that answer into the vocabulary of a users file. An edge deciding it by
/// matching on the AST itself is how the rule ends up written twice and enforced once.
impl From<Authority> for Role {
    fn from(authority: Authority) -> Self {
        match authority {
            Authority::Read => Self::Read,
            Authority::Write => Self::Write,
            Authority::Admin => Self::Admin,
        }
    }
}

/// What a request presented in its `Authorization` header.
///
/// Borrowed rather than owned because it lives exactly as long as the request it came out of,
/// and copying a password to hand it two functions down is one more copy to think about.
#[derive(Clone, Copy)]
pub struct Credential<'a> {
    /// The name, which is not a secret.
    pub user: &'a str,
    /// The password, which is.
    pub password: &'a str,
}

/// Redacted. A derived `Debug` would put a password into whatever printed it.
impl core::fmt::Debug for Credential<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Credential({}, redacted)", self.user)
    }
}

/// Who this server decided it is talking to.
///
/// Produced once per request and carried onward. Nothing downstream may ask again: asking again
/// means hashing again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// A person, holding a role.
    User {
        /// The name from the users file.
        name: String,
        /// What that name may do.
        role: Role,
    },
    /// Another node of this cluster, proven by its client certificate during the handshake.
    ///
    /// Carries no role, and that is the point: a role is a statement about a person's authority
    /// over data, and a node certificate is a statement about which process is speaking.
    Node {
        /// The node's name from the cluster file.
        name: String,
    },
    /// Authentication is switched off, which `big serve` permits only on a loopback bind.
    Anonymous,
}

impl Principal {
    /// Whether this principal reaches `needed`. A comparison, never a verification.
    ///
    /// `Err` carries what is held, so the caller can say both halves in a `403` - a message that
    /// names only what was required leaves the reader guessing what they have.
    pub fn holds(&self, needed: Role) -> Result<(), Role> {
        match self {
            // Auth is off, so everything is allowed - the existing contract, unchanged.
            Self::Anonymous => Ok(()),
            Self::User { role, .. } if *role >= needed => Ok(()),
            Self::User { role, .. } => Err(*role),
            // A node reaching a route that wants a role. Refused with the lowest role, because
            // there is no true answer: a certificate does not grant one. See `routes::Guard`,
            // where a peer is kept off these routes in the first place.
            Self::Node { .. } => Err(Role::Read),
        }
    }

    /// The name for the log line, and for nothing else.
    pub fn display(&self) -> &str {
        match self {
            Self::User { name, .. } | Self::Node { name } => name,
            Self::Anonymous => "-",
        }
    }

    /// Whether this is a peer of this cluster rather than a person.
    pub fn is_node(&self) -> bool {
        matches!(self, Self::Node { .. })
    }
}

/// The verdict on one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Allowed, carrying who it was allowed as - which is what the log records and what the
    /// per-statement check compares against.
    Allowed(Principal),
    /// No credential, or one this server does not know. `401`.
    Unauthenticated,
    /// A known credential that does not reach far enough. `403`.
    Forbidden {
        /// What the presented credential grants.
        held: Role,
        /// What the route required.
        needed: Role,
    },
    /// Every verification slot is busy. `503`, with a `Retry-After`.
    ///
    /// Its own variant rather than a `401`, because they mean opposite things to a client: a
    /// `401` says stop and fix your credentials, this says the credentials were never looked at.
    Overloaded,
}

/// The users table, and the decision of who may do what.
///
/// `Default` is disabled, which is safe only because binding anywhere but loopback without a
/// users file is refused by `big serve`.
#[derive(Clone, Default)]
pub struct Auth {
    /// `None` means authentication is switched off and every request is allowed.
    users: Option<Arc<Users>>,
}

struct Users {
    /// In file order, and looked up by comparing against all of them - see [`Auth::authorize`].
    entries: Vec<User>,
    /// A PHC string over random bytes, carrying the same cost as a real entry. Verified against
    /// when the username does not exist, so that "no such user" takes as long as "wrong
    /// password" and the pair of them is not a username oracle with a stopwatch.
    decoy: String,
    cache: Cache,
    throttle: Throttle,
    /// Injected so the cache's expiry is testable without a test that sleeps for a minute.
    now: fn() -> Instant,
}

struct User {
    name: String,
    phc: String,
    role: Role,
}

/// Redacted on purpose. A derived `Debug` would put every hash into whatever printed it - a
/// panic message, a test failure, an `{:?}` someone left in - and an argon2 hash in a log is an
/// offline attack somebody has been handed. The count is the part that is ever useful to see.
impl core::fmt::Debug for Auth {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.users {
            None => write!(f, "Auth(disabled)"),
            Some(u) => write!(f, "Auth({} users, redacted)", u.entries.len()),
        }
    }
}

impl Auth {
    /// Authentication off: every request is allowed, and every log line records no principal.
    pub fn disabled() -> Self {
        Self { users: None }
    }

    /// Whether a users file was loaded at all. Distinct from [`Auth::is_empty`]: a file that
    /// exists and grants nobody anything is enabled and empty, and refuses every request.
    pub fn is_enabled(&self) -> bool {
        self.users.is_some()
    }

    /// How many credentials are loaded. For the line the daemon prints on the way up.
    pub fn len(&self) -> usize {
        self.users.as_ref().map_or(0, |u| u.entries.len())
    }

    /// Whether no credentials are loaded, whether or not authentication is enabled.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sizes the verification ceiling against the worker pool.
    ///
    /// Called once by the server between loading the file and opening the port, because the
    /// ceiling is a fact about the pool and the pool is not known here. A quarter of the
    /// workers: enough that verification is never the bottleneck under honest load, few enough
    /// that a flood of wrong passwords cannot take every worker and every core with it.
    pub fn size_for(&self, workers: usize) {
        if let Some(u) = &self.users {
            u.throttle.ceiling.store((workers / 4).max(2), Ordering::Relaxed);
        }
    }

    /// Reads `username<space>role<space>hash` lines. `#` starts a comment; blank lines skipped.
    ///
    /// The same shape and the same comment rules as the token file this replaces, so the one
    /// thing an operator already knew about the format still holds.
    pub fn from_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        big_tls::mode::check_permissions(path, "a users file")?;
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text, &path.display().to_string())
    }

    /// The parser, separated from the file so that a test does not need a directory.
    pub fn parse(text: &str, whence: &str) -> io::Result<Self> {
        let bad = |n: usize, msg: String| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{whence}: line {}: {msg}", n + 1))
        };

        let mut entries: Vec<User> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(name), Some(role), Some(phc)) = (parts.next(), parts.next(), parts.next())
            else {
                return Err(bad(n, "expected `username role hash`".to_string()));
            };
            check_username(name).map_err(|e| bad(n, e))?;
            let Some(role) = Role::parse(role) else {
                return Err(bad(n, format!("`{role}` is not a role; use read, write or admin")));
            };
            // Parsed here so that the request path never has to decide what an unparseable hash
            // means. A file that would refuse everybody at runtime is refused at startup.
            hash::check(phc).map_err(|e| bad(n, e))?;
            // A duplicate would silently resolve to the last one, because the lookup below does
            // not stop at the first match. The token file tolerated duplicates; a duplicate user
            // is a configuration error and is worth naming both lines of.
            if let Some(first) = entries.iter().position(|u| u.name == name) {
                return Err(bad(n, format!("`{name}` is already on line {}", first + 1)));
            }
            entries.push(User { name: name.to_string(), phc: phc.to_string(), role });
        }

        if entries.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{whence}: no users; this would refuse every request"),
            ));
        }

        // Built with the first entry's cost parameters so that consulting it is indistinguishable
        // from a real verification. A file whose entries disagree about cost has a weaker
        // version of the same property, and the operator is told rather than left to find out.
        let decoy = hash::decoy_like(&entries[0].phc)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{whence}: {e}")))?;
        if let Some(odd) = entries.iter().find(|u| !hash::same_cost(&u.phc, &entries[0].phc)) {
            eprintln!(
                "big: `{}` in {whence} was hashed with different parameters from `{}`; \
                 the time a login takes will differ between them",
                odd.name, entries[0].name
            );
        }

        Ok(Self {
            users: Some(Arc::new(Users {
                entries,
                decoy,
                cache: Cache::new(),
                throttle: Throttle::new(),
                now: Instant::now,
            })),
        })
    }

    /// Whether `presented` - or what the connection already proved - may do something that
    /// needs `needed`.
    ///
    /// The order of the checks is the design:
    ///
    /// 1. **A node short-circuits.** It proved itself with a certificate during the handshake and
    ///    never sends a password, so the fan-out between nodes costs no hashing at all. That is
    ///    not an optimisation, it is a requirement: every internal message would otherwise pay
    ///    fifty milliseconds.
    /// 2. Authentication off allows everything, unchanged.
    /// 3. No credential is a `401` before any work is done.
    /// 4. The cache, which is what keeps a busy client from paying per request.
    /// 5. The throttle, which bounds how many verifications run at once.
    /// 6. Exactly one argon2 verification.
    pub fn authorize(
        &self,
        identity: &Identity,
        presented: Option<Credential<'_>>,
        needed: Role,
    ) -> Outcome {
        if let Identity::Node(name) = identity {
            return Outcome::Allowed(Principal::Node { name: name.clone() });
        }
        let Some(users) = &self.users else { return Outcome::Allowed(Principal::Anonymous) };
        let Some(presented) = presented else { return Outcome::Unauthenticated };

        let key = users.cache.key(presented.user, presented.password);
        if let Some(role) = users.cache.get(&key, (users.now)()) {
            return decide(presented.user, role, needed);
        }

        // **The name is compared against every entry with no early exit**, because a name is
        // cheap and where one sits in the file is not something a caller should be able to
        // learn. The password is then verified exactly *once* - against the entry that matched,
        // or against the decoy when none did. Verifying against every entry, which is what the
        // token table used to do, would be N hashes per request: not a defence, an amplifier.
        let mut found: Option<&User> = None;
        for u in &users.entries {
            if constant_time_eq(u.name.as_bytes(), presented.user.as_bytes()) {
                found = Some(u);
            }
        }

        let Some(_permit) = users.throttle.acquire() else { return Outcome::Overloaded };
        let phc = found.map_or(users.decoy.as_str(), |u| u.phc.as_str());
        let verified = hash::verify(phc, presented.password);

        match (found, verified) {
            (Some(u), true) => {
                // Only successes are remembered. Caching a failure would turn an offline
                // dictionary attack into an online one at the speed of a hash table, and the
                // cost of a wrong password *is* the rate limit.
                users.cache.put(key, u.role, (users.now)());
                decide(presented.user, u.role, needed)
            }
            _ => Outcome::Unauthenticated,
        }
    }

    /// How many verifications have been run, and how many requests the cache saved.
    ///
    /// For the metrics, which is where an operator sizing `--workers` looks: the verification
    /// rate is the number that explains a latency cliff.
    pub fn counters(&self) -> (u64, u64, u64) {
        match &self.users {
            None => (0, 0, 0),
            Some(u) => (
                u.cache.misses.load(Ordering::Relaxed),
                u.cache.hits.load(Ordering::Relaxed),
                u.throttle.refused.load(Ordering::Relaxed),
            ),
        }
    }

    /// Replaces the clock, for the one test that would otherwise have to sleep for a minute.
    #[cfg(test)]
    fn with_clock(mut self, now: fn() -> Instant) -> Self {
        let users = Arc::get_mut(self.users.as_mut().unwrap()).unwrap();
        users.now = now;
        self
    }
}

/// Turns a role that was found into the verdict for the role that was wanted.
fn decide(name: &str, held: Role, needed: Role) -> Outcome {
    if held >= needed {
        Outcome::Allowed(Principal::User { name: name.to_string(), role: held })
    } else {
        Outcome::Forbidden { held, needed }
    }
}

/// What a username may be.
///
/// The `:` ban is not needed by the file, which splits on whitespace. It is there so that
/// `user:password` in an `Authorization: Basic` header has exactly one reading - RFC 7617 splits
/// on the first colon, so a name containing one would let two different credentials produce the
/// same pair.
fn check_username(name: &str) -> Result<(), String> {
    if name.len() > 64 {
        return Err(format!("`{name}` is longer than 64 bytes"));
    }
    if let Some(c) = name.chars().find(|c| !c.is_ascii_graphic() || *c == ':') {
        return Err(format!("`{name}` contains `{c}`, which a username may not"));
    }
    Ok(())
}

/// Equal-length, equal-content, in time that does not depend on where they differ.
///
/// The length check is not constant time and does not need to be: a username's length is not the
/// secret, and comparing different lengths byte-for-byte would need a decision about what to
/// compare the short one against.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

const SHARDS: usize = 16;
const SLOTS: usize = 64;

/// How long a verified credential stays believed.
///
/// **Sixty seconds from when it was written, and never extended by use.** A sliding window would
/// let a credential that `big passwd` has just deleted keep working for as long as somebody kept
/// using it - which is exactly backwards, because it would make the busiest stolen password the
/// longest-lived one. This way a revocation takes effect within a minute without a restart, and
/// that minute is the whole of the exposure.
const TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct Slot {
    key: [u8; 16],
    role: Role,
    /// `None` is an empty slot. Also what an expired one is reset to.
    at: Option<Instant>,
}

/// Verified credentials, so that a busy client pays for argon2 once a minute rather than once a
/// request.
///
/// Fixed size and non-allocating: the critical section is a sixteen-byte comparison and a clock
/// read, with no eviction bookkeeping to do under the lock. Sharded so that a pool of workers
/// does not queue on one mutex.
struct Cache {
    shards: [Mutex<[Slot; SLOTS]>; SHARDS],
    /// Drawn once from the OS and never logged, persisted, or compared against anything. It
    /// makes a cache key that means nothing outside this process's address space. A core dump
    /// still leaks it - and a core dump already leaks the plaintext password sitting in the
    /// request buffer, so this is not the weakest link and does not pretend to be.
    key: [u8; 32],
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

impl Cache {
    fn new() -> Self {
        let empty = Slot { key: [0; 16], role: Role::Read, at: None };
        Self {
            shards: std::array::from_fn(|_| Mutex::new([empty; SLOTS])),
            key: hash::random_bytes(),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A key that is not the password, and not derivable from anything an attacker holds.
    ///
    /// The length of the username goes in before the username so that `("ab", "c")` and
    /// `("a", "bc")` cannot collide into one key - which would let one user's password
    /// authenticate as another.
    fn key(&self, user: &str, password: &str) -> [u8; 16] {
        let mut input = Vec::with_capacity(48 + user.len() + password.len());
        input.extend_from_slice(&self.key);
        input.extend_from_slice(&(user.len() as u64).to_le_bytes());
        input.extend_from_slice(user.as_bytes());
        input.extend_from_slice(password.as_bytes());
        hash::short_hash(&input)
    }

    fn slot(&self, key: &[u8; 16]) -> (usize, usize) {
        (key[0] as usize % SHARDS, key[1] as usize % SLOTS)
    }

    fn get(&self, key: &[u8; 16], now: Instant) -> Option<Role> {
        let (shard, slot) = self.slot(key);
        let mut slots = self.shards[shard].lock().expect("no panic holds this lock");
        let entry = &mut slots[slot];
        let fresh = entry.at.is_some_and(|at| now.duration_since(at) < TTL);
        if fresh && constant_time_eq(&entry.key, key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(entry.role);
        }
        if !fresh {
            *entry = Slot { key: [0; 16], role: Role::Read, at: None };
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Writes, overwriting whatever was there.
    ///
    /// A collision costs one legitimate user one argon2 verification, which is the correct thing
    /// to lose: the alternative is bookkeeping under a lock that every request takes.
    fn put(&self, key: [u8; 16], role: Role, now: Instant) {
        let (shard, slot) = self.slot(&key);
        let mut slots = self.shards[shard].lock().expect("no panic holds this lock");
        slots[slot] = Slot { key, role, at: Some(now) };
    }
}

// ---------------------------------------------------------------------------
// The throttle
// ---------------------------------------------------------------------------

/// A ceiling on how many argon2 verifications run at once.
///
/// **Not optional.** argon2's cost is memory as well as time - nineteen mebibytes per
/// verification at the default parameters - so without this, `workers` concurrent wrong
/// passwords pin every core *and* allocate over a gigabyte, bought with one TCP connection each.
/// The same shape as `Peer::slot` in `big-cluster`, because it is the same problem.
struct Throttle {
    in_flight: Mutex<usize>,
    wake: Condvar,
    ceiling: AtomicUsize,
    refused: std::sync::atomic::AtomicU64,
}

impl Throttle {
    fn new() -> Self {
        Self {
            in_flight: Mutex::new(0),
            wake: Condvar::new(),
            // Replaced by `Auth::size_for` once the worker count is known. Two is the floor, so
            // that an `Auth` used without a server - in a test, or embedded - still works.
            ceiling: AtomicUsize::new(2),
            refused: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A permit, or `None` if the wait ran out.
    ///
    /// Waiting rather than refusing immediately, because the thing being waited for takes
    /// milliseconds and a client that gets a `503` for a correct password has been told
    /// something false. One second is long enough to ride out a burst and short enough that a
    /// genuine flood is still shed rather than queued invisibly.
    fn acquire(&self) -> Option<Permit<'_>> {
        const WAIT: Duration = Duration::from_secs(1);
        let deadline = Instant::now() + WAIT;
        let mut n = self.in_flight.lock().expect("no panic holds this lock");
        loop {
            if *n < self.ceiling.load(Ordering::Relaxed) {
                *n += 1;
                return Some(Permit { throttle: self });
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                self.refused.fetch_add(1, Ordering::Relaxed);
                return None;
            };
            let (guard, timed_out) =
                self.wake.wait_timeout(n, left).expect("no panic holds this lock");
            n = guard;
            if timed_out.timed_out() && *n >= self.ceiling.load(Ordering::Relaxed) {
                self.refused.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
    }
}

/// Holds one verification slot and gives it back however the verification ends.
struct Permit<'a> {
    throttle: &'a Throttle,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut n = self.throttle.in_flight.lock().expect("no panic holds this lock");
        *n -= 1;
        self.throttle.wake.notify_one();
    }
}

// ---------------------------------------------------------------------------
// argon2, and the two other things that need a hash
// ---------------------------------------------------------------------------

/// Everything that touches the password hash, so that the rest of this file reads as policy.
mod hash {
    use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
    use argon2::Argon2;

    /// Refuses a hash this server would not be able to check later.
    ///
    /// Called at load time, so that the request path never has to decide what an unparseable or
    /// unsupported hash means. `argon2id` only: refusing `argon2i` and `argon2d` here means the
    /// choice between them is made once, by whoever wrote the file, rather than per request.
    pub fn check(phc: &str) -> Result<(), String> {
        let parsed = PasswordHash::new(phc).map_err(|e| format!("`{phc}` is not a hash: {e}"))?;
        if parsed.algorithm.as_str() != "argon2id" {
            return Err(format!(
                "`{}` is not argon2id; rehash this user with `big passwd`",
                parsed.algorithm
            ));
        }
        if parsed.hash.is_none() {
            return Err("this is a set of parameters with no hash in it".to_string());
        }
        Ok(())
    }

    /// Verifies, using whichever parameters the stored hash was made with.
    ///
    /// That is what lets a file hold a mix - a user hashed on an older, cheaper setting still
    /// works, and is rehashed the next time their password is set rather than being locked out.
    pub fn verify(phc: &str, password: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(phc) else { return false };
        Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
    }

    /// Hashes a new password at this build's default cost.
    ///
    /// `Params::DEFAULT` is OWASP's second profile: 19 MiB, two passes, one lane. The memory
    /// figure is an operational number, not a detail - it is per concurrent verification, and
    /// belongs in the runbook next to `--workers`.
    pub fn make(password: &str) -> Result<String, String> {
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| e.to_string())
    }

    /// A hash of random bytes carrying `like`'s cost, for the unknown-username path.
    pub fn decoy_like(like: &str) -> Result<String, String> {
        let parsed = PasswordHash::new(like).map_err(|e| e.to_string())?;
        let params = argon2::Params::try_from(&parsed).map_err(|e| e.to_string())?;
        let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        let secret: [u8; 32] = random_bytes();
        argon.hash_password(&secret, &salt).map(|h| h.to_string()).map_err(|e| e.to_string())
    }

    /// Whether two hashes would take the same time to check.
    pub fn same_cost(a: &str, b: &str) -> bool {
        let cost = |s: &str| {
            PasswordHash::new(s)
                .ok()
                .and_then(|p| argon2::Params::try_from(&p).ok())
                .map(|p| (p.m_cost(), p.t_cost(), p.p_cost()))
        };
        cost(a) == cost(b)
    }

    /// Bytes from the operating system's generator.
    pub fn random_bytes<const N: usize>() -> [u8; N] {
        use argon2::password_hash::rand_core::RngCore;
        let mut out = [0u8; N];
        argon2::password_hash::rand_core::OsRng.fill_bytes(&mut out);
        out
    }

    /// Sixteen bytes of blake2b, for the cache key.
    ///
    /// blake2 is already in the tree - argon2 is built on it - so this is a hash that costs no
    /// new dependency and about a hundred nanoseconds, against the fifty milliseconds it is
    /// there to avoid.
    pub fn short_hash(input: &[u8]) -> [u8; 16] {
        use blake2::digest::{Update, VariableOutput};
        let mut h = blake2::Blake2bVar::new(16).expect("16 is a valid blake2b length");
        h.update(input);
        let mut out = [0u8; 16];
        h.finalize_variable(&mut out).expect("the length matches the one above");
        out
    }
}

/// Hashes a password the way the users file stores them.
///
/// Public so `big passwd` can use it, which is what keeps `argon2` a dependency of this crate
/// alone. There is deliberately no route that writes a users file: one would let an `admin`
/// credential rewrite the credential file over the network.
pub fn hash_password(password: &str) -> io::Result<String> {
    hash::make(password).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Cheap parameters, so the suite is not paying OWASP's memory cost a hundred times. The
    /// verifier reads cost back out of each hash, so this needs no special API - which is
    /// itself the property being relied on.
    fn phc(password: &str) -> String {
        use argon2::password_hash::{PasswordHasher, SaltString};
        let params = argon2::Params::new(8, 1, 1, None).unwrap();
        let argon =
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let salt = SaltString::from_b64("c29tZXNhbHQ").unwrap();
        argon.hash_password(password.as_bytes(), &salt).unwrap().to_string()
    }

    fn users(lines: &[(&str, &str, &str)]) -> Auth {
        let text: String = lines.iter().map(|(n, r, p)| format!("{n} {r} {}\n", phc(p))).collect();
        Auth::parse(&text, "test").unwrap()
    }

    fn cred<'a>(user: &'a str, password: &'a str) -> Option<Credential<'a>> {
        Some(Credential { user, password })
    }

    #[test]
    fn disabled_allows_everything() {
        let a = Auth::disabled();
        assert_eq!(
            a.authorize(&Identity::None, None, Role::Admin),
            Outcome::Allowed(Principal::Anonymous)
        );
        assert!(!a.is_enabled());
    }

    #[test]
    fn a_role_contains_the_ones_below_it() {
        let a = users(&[("alice", "admin", "pw"), ("bob", "read", "pw")]);
        for needed in [Role::Read, Role::Write, Role::Admin] {
            assert!(matches!(
                a.authorize(&Identity::None, cred("alice", "pw"), needed),
                Outcome::Allowed(Principal::User { role: Role::Admin, .. })
            ));
        }
        assert!(matches!(
            a.authorize(&Identity::None, cred("bob", "pw"), Role::Read),
            Outcome::Allowed(Principal::User { role: Role::Read, .. })
        ));
    }

    #[test]
    fn a_short_role_is_forbidden_not_unauthenticated() {
        let a = users(&[("bob", "read", "pw")]);
        assert_eq!(
            a.authorize(&Identity::None, cred("bob", "pw"), Role::Write),
            Outcome::Forbidden { held: Role::Read, needed: Role::Write },
            "a known credential that does not reach far enough is a 403, not a 401 - retrying \
             with the same password will never work and the client should be told so"
        );
    }

    #[test]
    fn an_unknown_user_or_a_wrong_password_is_unauthenticated() {
        let a = users(&[("bob", "read", "pw")]);
        assert_eq!(a.authorize(&Identity::None, None, Role::Read), Outcome::Unauthenticated);
        assert_eq!(
            a.authorize(&Identity::None, cred("nobody", "pw"), Role::Read),
            Outcome::Unauthenticated
        );
        assert_eq!(
            a.authorize(&Identity::None, cred("bob", "wrong"), Role::Read),
            Outcome::Unauthenticated,
            "and the two are not distinguishable from the outside"
        );
    }

    #[test]
    fn an_unknown_user_consults_the_decoy() {
        // The property that stops the pair above being a username oracle: an unknown name must
        // cost a verification, not a lookup. Asserted through the counter, because the timing it
        // is really about cannot be asserted on a shared CI machine without flaking.
        let a = users(&[("bob", "read", "pw")]);
        let (before, _, _) = a.counters();
        let _ = a.authorize(&Identity::None, cred("nobody-at-all", "pw"), Role::Read);
        let (after, _, _) = a.counters();
        assert_eq!(after, before + 1, "an unknown username still paid for a hash");
    }

    #[test]
    fn a_node_never_pays_for_a_hash() {
        // The fan-out path. A peer proved itself in the handshake, so it short-circuits every
        // one of the six steps - and if it ever stopped doing so, every internal message in the
        // cluster would start costing fifty milliseconds.
        let a = users(&[("bob", "read", "pw")]);
        let (before, _, _) = a.counters();
        let node = Identity::Node("node-a".to_string());
        assert_eq!(
            a.authorize(&node, None, Role::Admin),
            Outcome::Allowed(Principal::Node { name: "node-a".to_string() })
        );
        let (after, _, _) = a.counters();
        assert_eq!(after, before, "no verification was run");
    }

    #[test]
    fn a_cached_verification_does_not_hash_again() {
        let a = users(&[("bob", "read", "pw")]);
        let _ = a.authorize(&Identity::None, cred("bob", "pw"), Role::Read);
        let (after_first, _, _) = a.counters();
        for _ in 0..5 {
            assert!(matches!(
                a.authorize(&Identity::None, cred("bob", "pw"), Role::Read),
                Outcome::Allowed(_)
            ));
        }
        let (after_rest, hits, _) = a.counters();
        assert_eq!(after_rest, after_first, "five more requests, no more hashing");
        assert_eq!(hits, 5);
    }

    #[test]
    fn a_wrong_password_is_never_cached() {
        // Caching failures would turn an offline dictionary attack into an online one at the
        // speed of a hash table. Every guess has to pay.
        let a = users(&[("bob", "read", "pw")]);
        for _ in 0..3 {
            assert_eq!(
                a.authorize(&Identity::None, cred("bob", "wrong"), Role::Read),
                Outcome::Unauthenticated
            );
        }
        let (verifications, hits, _) = a.counters();
        assert_eq!(verifications, 3, "each guess paid for itself");
        assert_eq!(hits, 0);
    }

    #[test]
    fn the_cache_forgets_after_its_ttl() {
        // Through an injected clock rather than a sleep, because the alternative is a test that
        // takes a minute and a suite nobody runs.
        use std::sync::atomic::AtomicU64;
        static OFFSET: AtomicU64 = AtomicU64::new(0);
        fn clock() -> Instant {
            // A fixed base plus however far the test has moved it, so that time only advances
            // when the test says so.
            static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            *BASE.get_or_init(Instant::now) + Duration::from_secs(OFFSET.load(Ordering::Relaxed))
        }

        OFFSET.store(0, Ordering::Relaxed);
        let a = users(&[("bob", "read", "pw")]).with_clock(clock);
        let _ = a.authorize(&Identity::None, cred("bob", "pw"), Role::Read);
        let (first, _, _) = a.counters();

        let _ = a.authorize(&Identity::None, cred("bob", "pw"), Role::Read);
        assert_eq!(a.counters().0, first, "still inside the window");

        OFFSET.store(61, Ordering::Relaxed);
        let _ = a.authorize(&Identity::None, cred("bob", "pw"), Role::Read);
        assert_eq!(a.counters().0, first + 1, "past the window, it hashes again");
    }

    #[test]
    fn two_users_cannot_collide_into_one_cache_key() {
        // `("ab", "c")` and `("a", "bc")` concatenate to the same bytes. Without the length
        // prefix they would share a slot, and one user's password would authenticate as the
        // other - which is the worst bug this file could have.
        let a = users(&[("ab", "admin", "c"), ("a", "read", "bc")]);
        assert!(matches!(
            a.authorize(&Identity::None, cred("ab", "c"), Role::Admin),
            Outcome::Allowed(Principal::User { role: Role::Admin, .. })
        ));
        assert_eq!(
            a.authorize(&Identity::None, cred("a", "bc"), Role::Admin),
            Outcome::Forbidden { held: Role::Read, needed: Role::Admin },
            "the second user is still only `read`"
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let text = format!(
            "# the ops team\n\n  alice admin {}  \n\nbob read {} # read only\n",
            phc("pw"),
            phc("pw")
        );
        let a = Auth::parse(&text, "test").unwrap();
        assert_eq!(a.len(), 2);
        assert!(matches!(
            a.authorize(&Identity::None, cred("bob", "pw"), Role::Read),
            Outcome::Allowed(_)
        ));
    }

    #[test]
    fn a_file_with_no_users_is_refused() {
        let e = Auth::parse("# nobody here\n", "test").unwrap_err();
        assert!(e.to_string().contains("no users"), "{e}");
    }

    #[test]
    fn a_duplicate_username_is_refused_and_names_both_lines() {
        // The token file tolerated duplicates. A duplicate user would silently resolve to the
        // last one, because the lookup does not stop at the first match.
        let text = format!("bob read {}\nbob admin {}\n", phc("pw"), phc("pw"));
        let e = Auth::parse(&text, "test").unwrap_err();
        assert!(e.to_string().contains("line 2"), "{e}");
        assert!(e.to_string().contains("already on line 1"), "{e}");
    }

    #[test]
    fn an_unknown_role_is_refused_by_name() {
        let text = format!("bob superuser {}\n", phc("pw"));
        let e = Auth::parse(&text, "test").unwrap_err();
        assert!(e.to_string().contains("superuser"), "{e}");
    }

    #[test]
    fn a_username_with_a_colon_is_refused() {
        // Not a rule the file needs - it splits on whitespace. It is there so `user:password` in
        // a Basic header has exactly one reading.
        let text = format!("bo:b read {}\n", phc("pw"));
        let e = Auth::parse(&text, "test").unwrap_err();
        assert!(e.to_string().contains("`:`"), "{e}");
    }

    #[test]
    fn a_hash_that_is_not_argon2id_is_refused_at_load_time() {
        // So that the request path never has to decide what an unsupported hash means.
        let text = "bob read $argon2i$v=19$m=8,t=1,p=1$c29tZXNhbHQ$aGFzaGhhc2hoYXNoaGFzaGhhc2g\n";
        let e = Auth::parse(text, "test").unwrap_err();
        assert!(e.to_string().contains("argon2id"), "{e}");
    }

    #[test]
    fn a_line_that_is_not_three_fields_says_what_it_wanted() {
        let e = Auth::parse("bob read\n", "test").unwrap_err();
        assert!(e.to_string().contains("username role hash"), "{e}");
    }

    #[test]
    fn a_world_readable_users_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "bob read {}", phc("pw")).unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let e = Auth::from_file(&path).unwrap_err();
            assert!(e.to_string().contains("chmod 600"), "{e}");
            assert!(e.to_string().contains("a users file"), "{e}");
        }
    }

    #[test]
    fn a_hash_round_trips_through_the_public_helper() {
        // What `big passwd` writes has to be what `Auth::parse` accepts. These are the two ends
        // of the same file and nothing else checks that they agree.
        let phc = hash_password("correct horse battery staple").unwrap();
        hash::check(&phc).unwrap();
        assert!(hash::verify(&phc, "correct horse battery staple"));
        assert!(!hash::verify(&phc, "Correct horse battery staple"));
        let a = Auth::parse(&format!("alice admin {phc}\n"), "test").unwrap();
        assert!(matches!(
            a.authorize(
                &Identity::None,
                cred("alice", "correct horse battery staple"),
                Role::Admin
            ),
            Outcome::Allowed(_)
        ));
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn the_throttle_hands_permits_back() {
        // A permit that leaked would take the ceiling down by one every time a verification
        // failed, and the server would stop authenticating anybody after four bad passwords.
        let a = users(&[("bob", "read", "pw")]);
        for _ in 0..20 {
            let _ = a.authorize(&Identity::None, cred("bob", "wrong"), Role::Read);
        }
        assert_eq!(a.counters().2, 0, "nothing was refused for want of a slot");
        assert!(matches!(
            a.authorize(&Identity::None, cred("bob", "pw"), Role::Read),
            Outcome::Allowed(_)
        ));
    }
}
