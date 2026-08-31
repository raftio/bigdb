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

//! Agreement, over the one thing that has to be agreed.
//!
//! **What is replicated here is the ownership map, and nothing else.** Not the facts, not the
//! row keys, not the schema - those have owners already and did not need a protocol. What
//! needed one is the single question static configuration could not answer: *which copy of
//! this range is the one to read from, now?* That is a value of a few dozen bytes that changes
//! when a machine dies, so the log this agrees on is short, cold, and never on the path of a
//! query.
//!
//! That framing is what makes hand-writing this defensible at all. A consensus protocol over
//! the write path would have to be fast, would have to snapshot, would have to compact, and
//! would be the largest and least tested thing in the tree. A consensus protocol over one
//! small value has none of those problems and still answers the question.
//!
//! **This module has no I/O and no clock.** Every decision is a function of the state, the
//! message and a timestamp handed in, and every effect comes back as a value. That is not a
//! stylistic choice: a leader election that can only be observed by starting five processes
//! and waiting is a leader election nobody can test, and the bugs here are all in the rules -
//! a vote granted to a stale log, a commit counted across terms - which are exactly the
//! things a deterministic test can pin down.
//!
//! The rules implemented are Raft's, in the shape the paper states them:
//!
//! - A message carrying a higher term makes this node a follower at that term, always.
//! - A message carrying a lower term is refused, always.
//! - A vote is granted at most once per term, and only to a log at least as up to date as
//!   this one.
//! - An append is refused unless the entry before it matches; a match truncates whatever
//!   disagreed.
//! - **A leader commits by counting replicas of an entry from its own term.** Counting an
//!   older entry's replicas is the classic way to commit something that a later leader then
//!   overwrites, and the no-op appended on election is what makes the first commit of a term
//!   reachable at all.

use std::collections::{BTreeMap, BTreeSet};

/// A node's index in the cluster file. Stable for the life of a process, which is the only
/// life this protocol has: membership changes are a restart, like every other config change.
pub type NodeId = usize;
pub type Term = u64;
/// One past the last index of the log. Index 0 is the sentinel that is always agreed.
pub type Index = u64;

/// Which node currently serves each range, and which copies are known to be behind.
///
/// Indexed by the range's position in the cluster file, so the *ranges* stay static and only
/// the answer to "who serves this one" moves. That is the whole difference between this and
/// rebalancing: no data moves, because every node in a range's group already holds it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Ownership {
    /// The node a read of each range goes to.
    pub primary: Vec<NodeId>,
    /// Copies that were unreachable at some point since they were last repaired, and may
    /// therefore have missed a write.
    ///
    /// **This is what lets a write survive a dead spare.** A write that could not reach a copy
    /// is allowed to stand, and the copy is recorded here; a copy recorded here is one the
    /// agreement will not promote. So nothing ever reads from a copy that is behind, and no
    /// write is refused because a machine nobody is reading from has died.
    ///
    /// Conservative on purpose: a node that was briefly unreachable and missed nothing is
    /// still marked, because the alternative is deciding whether a write happened during a
    /// window nobody was watching. A repair clears it.
    pub stale: Vec<NodeId>,
}

impl Ownership {
    pub fn is_stale(&self, node: NodeId) -> bool {
        self.stale.contains(&node)
    }
}

/// What one log entry decides.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Decides nothing. Appended by a new leader so that it has an entry of its own term to
    /// commit, which is what makes everything before it committable.
    Noop,
    /// From now on, these nodes serve these ranges.
    Own(Ownership),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub term: Term,
    pub decision: Decision,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

/// One message between nodes. Small enough to fit a request body with room to spare.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Message {
    RequestVote {
        term: Term,
        candidate: NodeId,
        last_index: Index,
        last_term: Term,
    },
    VoteReply {
        term: Term,
        from: NodeId,
        granted: bool,
    },
    Append {
        term: Term,
        leader: NodeId,
        prev_index: Index,
        prev_term: Term,
        entries: Vec<Entry>,
        commit: Index,
    },
    AppendReply {
        term: Term,
        from: NodeId,
        success: bool,
        match_index: Index,
    },
}

/// How long this node waits before doing anything about silence.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Shortest time without hearing from a leader before standing for election.
    pub election_min: u64,
    /// The window the randomised part of the timeout falls in. Two nodes that time out
    /// together split the vote and neither wins, so the spread is what makes an election end.
    pub election_spread: u64,
    /// How often a leader sends an append, empty or not. Well under `election_min`, or the
    /// followers depose a leader that is doing its job.
    pub heartbeat: u64,
}

/// Milliseconds. Deliberately unhurried: this protocol decides who serves a range after a
/// machine has died, and paying an extra second to be sure costs less than an election held
/// because a garbage collector paused.
///
/// Named constants rather than literals in `Default` so that the relationships between them
/// can be checked below. Numbers whose *ratios* are what makes them safe should not be three
/// literals nobody compares.
pub const ELECTION_MIN_MS: u64 = 1_500;
pub const ELECTION_SPREAD_MS: u64 = 1_500;
pub const HEARTBEAT_MS: u64 = 400;

// A leader that cannot heartbeat several times inside the shortest election timeout is a
// leader its followers depose while it is doing its job - and every deposition is a window
// where a range is not served. Three is the usual margin and the one this assumes.
const _: () = assert!(HEARTBEAT_MS * 3 <= ELECTION_MIN_MS);
// Without a spread, two nodes time out together, split the vote, and do it again. The
// randomised part is the whole reason an election ends.
const _: () = assert!(ELECTION_SPREAD_MS > 0);

impl Default for Timing {
    fn default() -> Self {
        Self {
            election_min: ELECTION_MIN_MS,
            election_spread: ELECTION_SPREAD_MS,
            heartbeat: HEARTBEAT_MS,
        }
    }
}

/// Everything a step produced. The caller does the I/O; this module does the deciding.
#[derive(Default, Debug)]
pub struct Output {
    /// Messages to send, each to one node.
    pub send: Vec<(NodeId, Message)>,
    /// Whether term, vote or log changed, and therefore has to reach the disk **before** any
    /// of `send` reaches the network. A vote that is sent and then forgotten in a restart is
    /// two votes in one term, which is two leaders.
    pub persist: bool,
    /// Decisions that are now committed, in order, for the caller to apply.
    pub applied: Vec<Decision>,
}

impl Output {
    fn to(&mut self, node: NodeId, m: Message) {
        self.send.push((node, m));
    }
}

/// One node's view of the agreement.
pub struct Raft {
    id: NodeId,
    /// Every member, this node included. Fixed for the life of the process.
    members: Vec<NodeId>,
    timing: Timing,

    // --- persistent: none of this may be lost in a restart ---
    term: Term,
    voted_for: Option<NodeId>,
    /// One-based. `log[0]` is the sentinel every node agrees on without being told.
    log: Vec<Entry>,

    // --- volatile ---
    role: Role,
    leader: Option<NodeId>,
    commit: Index,
    applied: Index,
    votes: BTreeSet<NodeId>,
    next: BTreeMap<NodeId, Index>,
    matched: BTreeMap<NodeId, Index>,

    // --- clocks, all in the caller's milliseconds ---
    election_at: u64,
    heartbeat_at: u64,
    /// When each member was last heard from. The failure detector, and it costs nothing: a
    /// protocol that already heartbeats knows who is answering.
    heard: BTreeMap<NodeId, u64>,
}

impl Raft {
    pub fn new(id: NodeId, members: Vec<NodeId>, timing: Timing, now: u64) -> Self {
        let mut raft = Self {
            id,
            members,
            timing,
            term: 0,
            voted_for: None,
            log: vec![Entry { term: 0, decision: Decision::Noop }],
            role: Role::Follower,
            leader: None,
            commit: 0,
            applied: 0,
            votes: BTreeSet::new(),
            next: BTreeMap::new(),
            matched: BTreeMap::new(),
            election_at: 0,
            heartbeat_at: 0,
            heard: BTreeMap::new(),
        };
        raft.reset_election(now);
        raft
    }

    /// Restores what was on disk. The three fields that may not be lost, and nothing else:
    /// everything volatile is rebuilt by the first heartbeat.
    pub fn restore(&mut self, term: Term, voted_for: Option<NodeId>, log: Vec<Entry>) {
        self.term = term;
        self.voted_for = voted_for;
        if !log.is_empty() {
            self.log = log;
        }
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    /// How many nodes are in the agreement, this one included.
    pub fn members(&self) -> usize {
        self.members.len()
    }

    pub fn term(&self) -> Term {
        self.term
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }

    /// Who this node believes is leading, which is not necessarily who is.
    pub fn leader(&self) -> Option<NodeId> {
        self.leader
    }

    pub fn voted_for(&self) -> Option<NodeId> {
        self.voted_for
    }

    pub fn log(&self) -> &[Entry] {
        &self.log
    }

    pub fn commit_index(&self) -> Index {
        self.commit
    }

    /// When this node last heard anything from `node`.
    pub fn last_heard(&self, node: NodeId) -> Option<u64> {
        self.heard.get(&node).copied()
    }

    fn last_index(&self) -> Index {
        self.log.len() as Index - 1
    }

    fn last_term(&self) -> Term {
        self.log.last().map_or(0, |e| e.term)
    }

    fn term_at(&self, index: Index) -> Option<Term> {
        self.log.get(index as usize).map(|e| e.term)
    }

    fn majority(&self) -> usize {
        self.members.len() / 2 + 1
    }

    /// The randomised election timeout.
    ///
    /// Derived from the node and the term rather than from a random number generator: two
    /// nodes must not wait the same length of time, and a test must be able to say what will
    /// happen. Mixing the term in means a split vote does not repeat itself at the same
    /// offsets in the next term, which is the property randomness was there for.
    fn timeout(&self) -> u64 {
        let mut h = (self.id as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ self.term;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 29;
        self.timing.election_min + h % self.timing.election_spread.max(1)
    }

    fn reset_election(&mut self, now: u64) {
        self.election_at = now + self.timeout();
    }

    /// A higher term is not an opinion. Whatever this node was doing, it is a follower now.
    fn observe(&mut self, term: Term, out: &mut Output) {
        if term > self.term {
            self.term = term;
            self.voted_for = None;
            self.role = Role::Follower;
            self.leader = None;
            self.votes.clear();
            out.persist = true;
        }
    }

    /// The clock moved. Everything time-driven happens here and nowhere else.
    pub fn tick(&mut self, now: u64) -> Output {
        let mut out = Output::default();
        match self.role {
            Role::Leader => {
                if now >= self.heartbeat_at {
                    self.heartbeat_at = now + self.timing.heartbeat;
                    for &peer in &self.members.clone() {
                        if peer != self.id {
                            self.send_append(peer, &mut out);
                        }
                    }
                }
            }
            Role::Follower | Role::Candidate => {
                if now >= self.election_at {
                    self.stand(now, &mut out);
                }
            }
        }
        out
    }

    /// Stands for election in the next term.
    ///
    /// A single-member cluster wins here and now, which is not a special case so much as the
    /// general one with a majority of one - and it is what lets a node that is alone in its
    /// config file work at all.
    fn stand(&mut self, now: u64, out: &mut Output) {
        self.term += 1;
        self.role = Role::Candidate;
        self.voted_for = Some(self.id);
        self.votes = BTreeSet::from([self.id]);
        self.leader = None;
        out.persist = true;
        self.reset_election(now);

        if self.votes.len() >= self.majority() {
            self.win(now, out);
            return;
        }
        for &peer in &self.members.clone() {
            if peer != self.id {
                out.to(
                    peer,
                    Message::RequestVote {
                        term: self.term,
                        candidate: self.id,
                        last_index: self.last_index(),
                        last_term: self.last_term(),
                    },
                );
            }
        }
    }

    fn win(&mut self, now: u64, out: &mut Output) {
        self.role = Role::Leader;
        self.leader = Some(self.id);
        self.next.clear();
        self.matched.clear();
        for &peer in &self.members {
            self.next.insert(peer, self.last_index() + 1);
            self.matched.insert(peer, 0);
        }
        self.matched.insert(self.id, self.last_index());

        // The entry that makes everything before it committable. Without it a leader whose
        // term produced no decisions can never count a majority for an entry of its own term,
        // and entries from earlier terms may not be committed by counting - so a cluster that
        // elects a leader and is then idle would never finish applying what it already agreed.
        self.log.push(Entry { term: self.term, decision: Decision::Noop });
        self.matched.insert(self.id, self.last_index());
        out.persist = true;

        self.heartbeat_at = now + self.timing.heartbeat;
        for &peer in &self.members.clone() {
            if peer != self.id {
                self.send_append(peer, out);
            }
        }
        self.advance_commit(out);
    }

    /// Proposes a decision. `None` when this node is not the leader, because a proposal has to
    /// go through one.
    pub fn propose(&mut self, decision: Decision) -> Option<Output> {
        if self.role != Role::Leader {
            return None;
        }
        let mut out = Output { persist: true, ..Default::default() };
        self.log.push(Entry { term: self.term, decision });
        self.matched.insert(self.id, self.last_index());
        for &peer in &self.members.clone() {
            if peer != self.id {
                self.send_append(peer, &mut out);
            }
        }
        self.advance_commit(&mut out);
        Some(out)
    }

    fn send_append(&mut self, peer: NodeId, out: &mut Output) {
        let next = self.next.get(&peer).copied().unwrap_or(self.last_index() + 1);
        let prev_index = next.saturating_sub(1);
        let prev_term = self.term_at(prev_index).unwrap_or(0);
        let entries = self.log.get(next as usize..).map(<[Entry]>::to_vec).unwrap_or_default();
        out.to(
            peer,
            Message::Append {
                term: self.term,
                leader: self.id,
                prev_index,
                prev_term,
                entries,
                commit: self.commit,
            },
        );
    }

    /// One message from another node.
    pub fn deliver(&mut self, m: Message, now: u64) -> Output {
        let mut out = Output::default();
        match m {
            Message::RequestVote { term, candidate, last_index, last_term } => {
                self.heard.insert(candidate, now);
                self.observe(term, &mut out);
                let granted = self.grant(term, candidate, last_index, last_term);
                if granted {
                    self.voted_for = Some(candidate);
                    out.persist = true;
                    // Only a granted vote resets the timer. Resetting it on every request would
                    // let a node that keeps standing and keeps losing hold off a healthy
                    // election indefinitely.
                    self.reset_election(now);
                }
                out.to(candidate, Message::VoteReply { term: self.term, from: self.id, granted });
            }

            Message::VoteReply { term, from, granted } => {
                self.heard.insert(from, now);
                self.observe(term, &mut out);
                // A reply from an election this node has moved on from decides nothing.
                if self.role == Role::Candidate && term == self.term && granted {
                    self.votes.insert(from);
                    if self.votes.len() >= self.majority() {
                        self.win(now, &mut out);
                    }
                }
            }

            Message::Append { term, leader, prev_index, prev_term, entries, commit } => {
                self.heard.insert(leader, now);
                self.observe(term, &mut out);
                if term < self.term {
                    out.to(
                        leader,
                        Message::AppendReply {
                            term: self.term,
                            from: self.id,
                            success: false,
                            match_index: 0,
                        },
                    );
                    return out;
                }

                // A leader of this term exists, so this node is not standing for election in it.
                self.role = Role::Follower;
                self.leader = Some(leader);
                self.reset_election(now);

                if self.term_at(prev_index) != Some(prev_term) {
                    out.to(
                        leader,
                        Message::AppendReply {
                            term: self.term,
                            from: self.id,
                            success: false,
                            match_index: 0,
                        },
                    );
                    return out;
                }

                // Everything after the matching point is the leader's to decide. Entries that
                // agree are left alone rather than rewritten, so a heartbeat carrying a repeat
                // of what is already there does not truncate a log that is ahead of `commit`.
                let mut index = prev_index;
                for entry in entries {
                    index += 1;
                    match self.term_at(index) {
                        Some(t) if t == entry.term => continue,
                        Some(_) => {
                            self.log.truncate(index as usize);
                            self.log.push(entry);
                            out.persist = true;
                        }
                        None => {
                            self.log.push(entry);
                            out.persist = true;
                        }
                    }
                }

                if commit > self.commit {
                    self.commit = commit.min(self.last_index());
                    self.apply(&mut out);
                }
                out.to(
                    leader,
                    Message::AppendReply {
                        term: self.term,
                        from: self.id,
                        success: true,
                        match_index: index,
                    },
                );
            }

            Message::AppendReply { term, from, success, match_index } => {
                self.heard.insert(from, now);
                self.observe(term, &mut out);
                if self.role != Role::Leader || term != self.term {
                    return out;
                }
                if success {
                    self.matched.insert(from, match_index);
                    self.next.insert(from, match_index + 1);
                    self.advance_commit(&mut out);
                } else {
                    // Back up one and try again. Slower than the paper's optimisation and
                    // enough: a follower is behind by the entries of one failed term, and this
                    // log holds one entry per machine that has died.
                    let next = self.next.entry(from).or_insert(1);
                    *next = (*next).saturating_sub(1).max(1);
                    self.send_append(from, &mut out);
                }
            }
        }
        out
    }

    /// The highest index a majority holds, **of this term**.
    ///
    /// The term check is the one that matters. Counting replicas of an entry from an earlier
    /// term can commit something a later leader is still entitled to overwrite, which is the
    /// bug that Raft's figure 8 exists to show.
    fn advance_commit(&mut self, out: &mut Output) {
        let mut candidate = self.commit;
        for index in (self.commit + 1)..=self.last_index() {
            if self.term_at(index) != Some(self.term) {
                continue;
            }
            let replicas = self
                .members
                .iter()
                .filter(|m| self.matched.get(m).is_some_and(|x| *x >= index))
                .count();
            if replicas >= self.majority() {
                candidate = index;
            }
        }
        if candidate > self.commit {
            self.commit = candidate;
            self.apply(out);
        }
    }

    fn apply(&mut self, out: &mut Output) {
        while self.applied < self.commit {
            self.applied += 1;
            if let Some(entry) = self.log.get(self.applied as usize) {
                out.applied.push(entry.decision.clone());
            }
        }
    }

    /// Raft's vote rule, in one place: at most one per term, and never to a log that is behind.
    ///
    /// "Behind" is by last term first and length second. Length alone would let a node with a
    /// long log of entries nobody committed win over one with the entries that were.
    fn grant(&self, term: Term, candidate: NodeId, last_index: Index, last_term: Term) -> bool {
        if term < self.term {
            return false;
        }
        if self.voted_for.is_some_and(|v| v != candidate) {
            return false;
        }
        last_term > self.last_term()
            || (last_term == self.last_term() && last_index >= self.last_index())
    }
}

/// Where the three things that may not be lost are kept.
///
/// A vote that is sent and then forgotten in a restart is two votes in one term, which is two
/// leaders, which is the one failure this protocol exists to prevent. So this is not a cache
/// and it is not optional: a node that cannot persist may not vote.
///
/// The whole log is rewritten on every save, and that is affordable because of what the log
/// holds: one entry per election and one per machine that has died. It is not the write path
/// and it never will be.
/// Term, vote and log: the three things a restart may not lose.
pub type State = (Term, Option<NodeId>, Vec<Entry>);

pub trait Store: Send + Sync {
    fn load(&self) -> std::io::Result<Option<State>>;
    fn save(&self, term: Term, voted_for: Option<NodeId>, log: &[Entry]) -> std::io::Result<()>;
}

/// Keeps nothing.
///
/// For a single-node cluster, where there is nobody to give a second vote to, and for tests.
/// **Not for a real member of a real group**: a node that forgets its vote can vote twice.
pub struct Forgetful;

impl Store for Forgetful {
    fn load(&self) -> std::io::Result<Option<State>> {
        Ok(None)
    }

    fn save(&self, _: Term, _: Option<NodeId>, _: &[Entry]) -> std::io::Result<()> {
        Ok(())
    }
}

/// One small file, written whole and renamed into place.
///
/// Rename rather than rewrite, and fsync before the rename: a torn state file is a node that
/// cannot start, and this is the same reasoning the pager applies to its meta page one layer
/// down - there is a moment before the rename when nothing has changed, and a moment after it
/// when everything has, and no moment in between.
pub struct FileStore {
    path: std::path::PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

const MAGIC: &[u8; 8] = b"BIGRAFT1";

impl Store for FileStore {
    fn load(&self) -> std::io::Result<Option<State>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        decode_state(&bytes).map(Some).map_err(|why| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {why}", self.path.display()),
            )
        })
    }

    fn save(&self, term: Term, voted_for: Option<NodeId>, log: &[Entry]) -> std::io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let bytes = encode_state(term, voted_for, log);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        // The rename itself has to reach the disk, or a crash can leave the old file in place
        // having reported the new one written.
        if let Some(dir) = self.path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn encode_state(term: Term, voted_for: Option<NodeId>, log: &[Entry]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    put_u64(&mut out, term);
    put_u64(&mut out, voted_for.map_or(u64::MAX, |v| v as u64));
    put_u64(&mut out, log.len() as u64);
    for entry in log {
        put_u64(&mut out, entry.term);
        match &entry.decision {
            Decision::Noop => out.push(0),
            Decision::Own(o) => {
                out.push(1);
                put_u64(&mut out, o.primary.len() as u64);
                for p in &o.primary {
                    put_u64(&mut out, *p as u64);
                }
                put_u64(&mut out, o.stale.len() as u64);
                for n in &o.stale {
                    put_u64(&mut out, *n as u64);
                }
            }
        }
    }
    out
}

fn decode_state(bytes: &[u8]) -> Result<(Term, Option<NodeId>, Vec<Entry>), &'static str> {
    let mut at = 8usize;
    let u64_at = |at: &mut usize| -> Result<u64, &'static str> {
        let end = at.checked_add(8).ok_or("truncated")?;
        let slice = bytes.get(*at..end).ok_or("truncated")?;
        *at = end;
        Ok(u64::from_le_bytes(slice.try_into().expect("eight bytes")))
    };

    if bytes.get(..8) != Some(MAGIC) {
        return Err("not a raft state file");
    }
    let term = u64_at(&mut at)?;
    let voted = u64_at(&mut at)?;
    let voted_for = (voted != u64::MAX).then_some(voted as NodeId);
    let n = u64_at(&mut at)? as usize;
    // A count is four bytes of somebody else's file; every entry costs at least nine, so a
    // count the file cannot hold is a count that is not going to be allocated.
    if n > bytes.len() {
        return Err("truncated");
    }

    let mut log = Vec::with_capacity(n);
    for _ in 0..n {
        let term = u64_at(&mut at)?;
        let tag = *bytes.get(at).ok_or("truncated")?;
        at += 1;
        let decision = match tag {
            0 => Decision::Noop,
            1 => {
                let list = |at: &mut usize| -> Result<Vec<NodeId>, &'static str> {
                    let count = u64_at(at)? as usize;
                    if count > bytes.len() {
                        return Err("truncated");
                    }
                    let mut out = Vec::with_capacity(count);
                    for _ in 0..count {
                        out.push(u64_at(at)? as NodeId);
                    }
                    Ok(out)
                };
                let primary = list(&mut at)?;
                let stale = list(&mut at)?;
                Decision::Own(Ownership { primary, stale })
            }
            _ => return Err("unknown decision"),
        };
        log.push(Entry { term, decision });
    }
    if at != bytes.len() {
        return Err("bytes after the end");
    }
    Ok((term, voted_for, log))
}
