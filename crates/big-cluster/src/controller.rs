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

//! The thing that notices a machine has died and does something about it.
//!
//! Three jobs, and they are one job: the node that wins the agreement in [`crate::raft`] is
//! also the node that watches who is answering, and the same heartbeats do both. A protocol
//! that already has to say "I am here" every four hundred milliseconds knows who has stopped
//! saying it, so the failure detector is free.
//!
//! **Only started when a range has a copy.** A range with one node cannot fail over to
//! anything, so a cluster with no replicas runs no agreement at all and behaves exactly as it
//! did before this module existed - which is also what keeps the un-clustered case honest.
//!
//! **A promotion is safe because of a lease, not because of a timeout.** Two nodes serving one
//! range would each answer half of every query and neither would say so, and a failure
//! detector alone cannot rule that out: a node the leader cannot reach may be perfectly
//! healthy and still serving the clients that can reach it. So a node serves a *replicated*
//! range only while it has heard from the agreement recently, and a promotion is proposed only
//! after silence long enough that the old primary must already have stopped. The assumption is
//! that clocks run at roughly the same rate, which is the assumption every lease makes, and it
//! is written here rather than left to be discovered.
//!
//! **Failing back is not a thing.** When a node comes back it does not take its range again.
//! Ownership moves when it has to and stays where it lands, because a node that flaps would
//! otherwise move the range on every flap, and each move is a moment where a query can fail.

use crate::client::{Peers, Repeatable};
use crate::config::ClusterConfig;
use crate::raft::{self, Decision, Member, Message, NodeId, Raft, RangeMap, Store, Timing};
use crate::{path, wire};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// How long a node keeps serving a replicated range after the agreement goes quiet, and how
/// long the agreement waits before giving that range to somebody else.
#[derive(Clone, Copy, Debug)]
pub struct Leases {
    /// A node stops serving a replicated range this long after it last heard from the leader.
    pub serve_for: Duration,
    /// The leader proposes a promotion after this long without hearing from a primary.
    ///
    /// Longer than `serve_for`, and the margin is the safety: the old primary has to have
    /// stopped before the new one starts, and the only thing making that true is this
    /// inequality plus clocks that run at similar rates.
    pub promote_after: Duration,
}

impl Default for Leases {
    fn default() -> Self {
        Self {
            serve_for: Duration::from_millis(1_500),
            promote_after: Duration::from_millis(4_500),
        }
    }
}

/// One node's half of the agreement, and the thread that drives it.
pub struct Controller {
    raft: Mutex<Raft>,
    store: Box<dyn Store>,
    leases: Leases,
    /// The map, as last committed. Read on the path of every request, so it is a lock held
    /// for a pointer's worth of time and never across any I/O.
    ///
    /// **Shared with the [`crate::Cluster`] that owns this controller**, so that routing has
    /// one source rather than two that can drift. Seeded from the cluster file and replaced by
    /// every committed `Decision::Ranges`.
    ranges: Arc<RwLock<RangeMap>>,
    /// Who the cluster is, as last *appended* - the rule Raft states for a configuration
    /// change, because the majority that commits an entry has to be the one it describes.
    members: Arc<RwLock<Vec<Member>>>,
    /// This node's own index, kept because half the questions below are about it.
    this: NodeId,
    /// How the other nodes are reached. Held so that a node which joins can be added to it.
    peers: Arc<dyn Peers>,
    /// Milliseconds since this process started, at the last moment this node could prove it
    /// was still in touch with a majority. The lease, in one number.
    lease_at: AtomicU64,
    started: Instant,
    inbox: mpsc::Sender<Message>,
    /// One sender thread per peer, made on demand.
    outbox: Outbox,
    stop: Arc<AtomicBool>,
}

/// The threads that carry the agreement to the other nodes.
///
/// **One per peer, made when the peer first appears.** It used to be a fixed vector built from
/// the cluster file, so a message addressed to a node that joined at runtime went into
/// `outbox.get(to)`, matched `None`, and was silently dropped - a new node that could never be
/// reached and no error anywhere saying so.
struct Outbox {
    senders: RwLock<Vec<Option<mpsc::SyncSender<Message>>>>,
    peers: Arc<dyn Peers>,
    stop: Arc<AtomicBool>,
    budget: u64,
    this: NodeId,
}

impl Outbox {
    fn new(peers: Arc<dyn Peers>, stop: Arc<AtomicBool>, budget: u64, this: NodeId) -> Self {
        Self { senders: RwLock::new(Vec::new()), peers, stop, budget, this }
    }

    /// Makes sure there is a thread for every node below `len`.
    fn reach(&self, len: usize) {
        let mut senders = self.senders.write().expect("no panic holds this lock");
        if senders.len() < len {
            senders.resize_with(len, || None);
        }
        for i in 0..senders.len() {
            if i == self.this || senders[i].is_some() {
                continue;
            }
            // A short queue, and a heartbeat that cannot be queued is dropped rather than
            // waited for: this protocol was designed for a network that loses messages, and
            // blocking the one thread that drives it in order to reach a node that is not
            // answering is how a live cluster is brought down by a dead node.
            let (send, recv) = mpsc::sync_channel::<Message>(64);
            senders[i] = Some(send);
            let peers = Arc::clone(&self.peers);
            let stop = Arc::clone(&self.stop);
            let budget = self.budget;
            std::thread::Builder::new()
                .name(format!("big-raft-out-{i}"))
                .spawn(move || {
                    while let Ok(m) = recv.recv() {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let body = wire::encode_raft(&m);
                        let _ = peers.post(
                            i,
                            path::RAFT,
                            &body,
                            Some(Duration::from_millis(budget)),
                            // A message that arrives twice decides nothing twice: a term and an
                            // index say what a message means, so a retry on a connection that
                            // was closed underneath us is free.
                            Repeatable::Yes,
                        );
                    }
                })
                .expect("a thread per peer");
        }
    }

    /// How many nodes this outbox can already reach.
    fn len(&self) -> usize {
        self.senders.read().expect("no panic holds this lock").len()
    }

    fn send(&self, to: NodeId, m: Message) {
        let senders = self.senders.read().expect("no panic holds this lock");
        if let Some(Some(sender)) = senders.get(to) {
            // Full means this peer is not keeping up, and a queued heartbeat that is already
            // stale helps nobody.
            let _ = sender.try_send(m);
        }
    }
}

impl Controller {
    /// Starts the agreement, and the threads that carry it.
    ///
    /// `peers` is the same table everything else uses: agreement is traffic like any other,
    /// over the same routes with the same tokens. A trait object so that a test can drive an
    /// election without opening a socket.
    /// **A state file that cannot be read stops this node**, rather than being ignored. Reading
    /// it is how a node remembers the vote it already cast, so a node that starts without it
    /// starts at term 0 with an empty log and is free to vote a second time in a term it has
    /// already voted in - which is two leaders, the one failure this module exists to prevent.
    /// A missing file is not that: it is a node that has never voted, and `load` says so with
    /// `Ok(None)`. Only a file that exists and cannot be decoded lands here.
    pub fn start(
        config: &ClusterConfig,
        peers: Arc<dyn Peers>,
        store: Box<dyn Store>,
        timing: Timing,
        leases: Leases,
        ranges: Arc<RwLock<RangeMap>>,
    ) -> std::io::Result<Arc<Self>> {
        let started = Instant::now();
        let this = config.this_index();
        let mut raft = Raft::new(this, config.seed_members(), timing, 0);
        if let Some(state) = store.load()? {
            raft.restore(state);
        }

        // **The file seeds; the log decides.** Both are replayed from what is on disk rather
        // than taken from the file alone, or a node that restarted would come back believing
        // the file's map - which, once a range has moved, describes a cluster that no longer
        // exists. Members follow every entry because a configuration change applies when it is
        // appended; ranges follow only committed ones.
        let mut members = config.seed_members();
        {
            let mut map = ranges.write().expect("no panic holds this lock");
            for (i, entry) in raft.log().iter().enumerate() {
                match &entry.decision {
                    Decision::Members(ms) => members = ms.clone(),
                    Decision::Ranges(m) if i as u64 <= raft.commit_index() => *map = m.clone(),
                    _ => {}
                }
            }
        }

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        // One sender thread per peer, each with a short queue. A heartbeat that cannot be
        // queued is dropped rather than waited for: this protocol was designed for a network
        // that loses messages, and blocking the one thread that drives it in order to reach a
        // node that is not answering is how a live cluster is brought down by a dead node.
        let outbox = Outbox::new(Arc::clone(&peers), Arc::clone(&stop), timing.heartbeat, this);
        outbox.reach(peers.len());

        let controller = Arc::new(Self {
            raft: Mutex::new(raft),
            store,
            leases,
            ranges,
            members: Arc::new(RwLock::new(members)),
            this,
            peers: Arc::clone(&peers),
            lease_at: AtomicU64::new(0),
            started,
            inbox: tx,
            outbox,
            stop: Arc::clone(&stop),
        });

        let driver = Arc::clone(&controller);
        std::thread::Builder::new()
            .name("big-raft".to_string())
            .spawn(move || driver.run(rx))
            .expect("one driver thread");
        Ok(controller)
    }

    /// The map, as last committed.
    pub fn map(&self) -> RangeMap {
        self.ranges.read().expect("no panic holds this lock").clone()
    }

    /// Who the cluster is, as last appended.
    pub fn members(&self) -> Vec<Member> {
        self.members.read().expect("no panic holds this lock").clone()
    }

    /// Every range this node holds, whether it serves it or only copies it.
    pub fn ranges_held(&self) -> Vec<usize> {
        self.map().held_by(self.this)
    }

    /// Whether this node may answer as the primary of the ranges it serves right now.
    ///
    /// `true` when nothing it holds could be taken away. Otherwise it is the lease: this node
    /// stopped hearing from the agreement, so it has to assume it may already have been
    /// replaced, and answering would be the two-primaries failure this is all built to avoid.
    ///
    /// **Fencing is recomputed rather than fixed at startup.** It used to be a bool decided
    /// once from the file, which was exact while a node held exactly one range for the life of
    /// the process. A node can now gain and lose ranges, so whether it has anything worth
    /// fencing is a question about the map as it stands.
    pub fn may_serve(&self) -> bool {
        if !self.fenced() {
            return true;
        }
        self.since_lease() < self.leases.serve_for.as_millis() as u64
    }

    /// Whether any range this node holds has a copy that could take it.
    ///
    /// A range with no copy is never fenced: nothing could take it away, so a node that stops
    /// hearing from the agreement has lost nothing, and stopping would be an outage invented
    /// rather than avoided.
    fn fenced(&self) -> bool {
        let map = self.ranges.read().expect("no panic holds this lock");
        map.ranges.iter().any(|r| r.group.contains(&self.this) && r.group.len() > 1)
    }

    fn since_lease(&self) -> u64 {
        self.now().saturating_sub(self.lease_at.load(Ordering::Relaxed))
    }

    /// A message from a peer, on its way to the one thread that decides.
    ///
    /// Queued rather than handled here: every decision this protocol makes happens on one
    /// thread, so a message arriving on an HTTP worker cannot race a heartbeat.
    pub fn deliver(&self, m: Message) {
        let _ = self.inbox.send(m);
    }

    pub fn is_leader(&self) -> bool {
        self.raft.lock().expect("no panic holds this lock").is_leader()
    }

    pub fn term(&self) -> u64 {
        self.raft.lock().expect("no panic holds this lock").term()
    }

    /// Who the agreement currently answers to, as this node understands it.
    pub fn leader(&self) -> Option<NodeId> {
        self.raft.lock().expect("no panic holds this lock").leader()
    }

    /// Stops the threads. Only a test has a reason to: a daemon runs until it is killed.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn now(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    /// The one thread that decides anything.
    fn run(self: Arc<Self>, rx: mpsc::Receiver<Message>) {
        let tick = Duration::from_millis(50);
        while !self.stop.load(Ordering::Relaxed) {
            let incoming = rx.recv_timeout(tick).ok();
            let now = self.now();

            let (out, persist_state) = {
                let mut raft = self.raft.lock().expect("no panic holds this lock");
                let mut out = raft::Output::default();
                if let Some(m) = incoming {
                    let got = raft.deliver(m, now);
                    merge(&mut out, got);
                }
                merge(&mut out, raft.tick(now));

                // The lease. A follower renews it by hearing from the leader; a leader renews
                // it by hearing from a majority, which is the only proof it is still one.
                if let Some(anchor) = lease_anchor(&raft, now) {
                    self.lease_at.store(anchor, Ordering::Relaxed);
                }

                if raft.is_leader() {
                    if let Some(decision) = self.promotion(&raft, now) {
                        if let Some(proposed) = raft.propose(decision) {
                            merge(&mut out, proposed);
                        }
                    }
                }

                let state = out.persist.then(|| raft.state());
                (out, state)
            };

            // **Before anything is sent.** A vote that reaches the network and not the disk is
            // a vote this node can cast again after a restart, which is two leaders in one
            // term. The lock is released first because the disk is slow and the protocol is
            // not; nothing else writes this state, so there is nothing to race with.
            if let Some(state) = persist_state {
                if let Err(e) = self.store.save(&state) {
                    // A node that cannot persist may not participate. Dropping the messages is
                    // what makes that true: it looks exactly like a node that is down, which
                    // is the one failure every other node here already handles.
                    crate::log_persist_failure(&e);
                    continue;
                }
            }

            // **Before anything is sent to a node that may be new.** The membership the raft
            // core is working from is derived from its log, so a node that joined is already
            // being addressed - and a message to a peer with no client and no thread would be
            // dropped without a word. Cheap: both calls do nothing at all once the tables are
            // long enough, which is every tick but the few where the cluster changed.
            let membership =
                self.raft.lock().expect("no panic holds this lock").membership().to_vec();
            if membership.len() > self.outbox.len() {
                let addrs: Vec<(String, String)> =
                    membership.iter().map(|m| (m.name.clone(), m.addr.clone())).collect();
                self.peers.extend(&addrs);
                self.outbox.reach(membership.len());
            }
            *self.members.write().expect("no panic holds this lock") = membership;

            for decision in out.applied {
                match decision {
                    // Applied on commit: routing a read to a node before a majority agreed it
                    // owns the range would answer from a node nobody has acknowledged.
                    Decision::Ranges(m) => {
                        *self.ranges.write().expect("no panic holds this lock") = m
                    }
                    // Membership is applied when the entry is *appended*, not here - the raft
                    // core does it, and the loop above copies the result out every tick. A
                    // committed one decides nothing new.
                    Decision::Members(_) => {}
                    Decision::Noop => {}
                }
            }

            for (to, m) in out.send {
                self.outbox.send(to, m);
            }
        }
    }

    /// The change to ownership this node would propose, if any.
    ///
    /// Two things, and they are the same walk. A node that has stopped answering **is marked
    /// behind**, because a write may have been sent while it was gone and allowed to stand
    /// without it. A range whose serving copy has stopped answering **moves to a copy that is
    /// not marked behind**, which is what makes the first thing safe: the copy that missed the
    /// write is exactly the copy that will not be promoted.
    ///
    /// Only the agreement's leader gets here, and only when everything it has proposed is
    /// already committed - a second proposal while the first is in flight would be a decision
    /// made about a state that has not settled.
    fn promotion(&self, raft: &Raft, now: u64) -> Option<Decision> {
        if raft.commit_index() != raft.log().len() as u64 - 1 {
            return None;
        }
        let promote_after = self.leases.promote_after.as_millis() as u64;
        // A node nobody has heard from *yet* is not a node that has gone: at startup nothing
        // has been heard from anybody, and marking every peer behind before the first
        // heartbeat would mean a cluster that has to be repaired the moment it starts.
        let live = |node: NodeId| {
            node == raft.id()
                || now.saturating_sub(raft.last_heard(node).unwrap_or(0)) < promote_after
        };

        let mut next = self.map();
        let mut changed = false;

        // Marked behind, and never unmarked here: only a repair knows whether a copy has
        // caught up, and the agreement is not the thing that would find out.
        //
        // Only inside a group that has more than one copy. A range held by one node cannot be
        // given to anybody, so marking its node behind would record a fact nothing acts on and
        // put a name in a report that has no repair to run.
        let held: Vec<Vec<NodeId>> =
            next.ranges.iter().filter(|r| r.group.len() > 1).map(|r| r.group.clone()).collect();
        for group in &held {
            for &node in group {
                if !live(node) && !next.is_stale(node) {
                    next.stale.push(node);
                    changed = true;
                }
            }
        }
        next.stale.sort_unstable();

        for i in 0..next.ranges.len() {
            if live(next.ranges[i].primary) {
                continue;
            }
            // A copy that is answering *and* has not missed a write. In group order, so two
            // leaders elected in sequence make the same choice and the range does not move
            // twice. When there is no such copy the range stays where it is and stays
            // unavailable, which is the honest answer: promoting a copy that is behind would
            // answer from data somebody else has and this one does not.
            let replacement =
                next.ranges[i].group.iter().copied().find(|n| live(*n) && !next.is_stale(*n));
            let Some(replacement) = replacement else { continue };
            next.ranges[i].primary = replacement;
            changed = true;
        }

        // **A node that is serving keeps its mark.** It is tempting to clear one - the serving
        // copy is the truth by construction, so what could it be behind? - and it would be
        // wrong twice over. It prevents nothing: a mark is only ever read when *choosing* a
        // replacement, and the node being replaced is by then not a candidate. And it hides
        // the one case worth seeing, which is a node that came back with a replaced disk and
        // is now serving a range it no longer holds. `GET /verify` is how that is found; the
        // mark is what makes somebody look.

        if !changed {
            return None;
        }
        // A promotion never reshapes the space, so this cannot fail - but the check is here
        // rather than assumed, because every proposal goes through it and a proposal that
        // skipped it would be the one that got it wrong.
        if let Err(e) = next.check() {
            debug_assert!(false, "a promotion produced an invalid map: {e}");
            return None;
        }
        next.epoch += 1;
        Some(Decision::Ranges(next))
    }

    /// Proposes a new map, which every node will adopt once a majority has it.
    ///
    /// **Only the leader, and only when everything before it has settled.** A second proposal
    /// while the first is in flight would be a decision made about a state that has not landed
    /// - the same rule `promotion` follows, for the same reason.
    ///
    /// The map is checked before it is proposed rather than after it commits, because a
    /// committed map that leaves a gap is a record id nobody answers for and there is nothing
    /// downstream that would notice.
    pub fn propose_map(&self, mut next: RangeMap) -> core::result::Result<u64, ProposeError> {
        next.check().map_err(ProposeError::Invalid)?;
        let mut raft = self.raft.lock().expect("no panic holds this lock");
        if !raft.is_leader() {
            return Err(ProposeError::NotLeader { leader: raft.leader() });
        }
        if raft.commit_index() != raft.last_index() {
            return Err(ProposeError::Busy);
        }
        // Bumped here rather than by the caller, so that two callers racing cannot mint one
        // epoch twice - and a routed request carrying an epoch is only useful if it counts.
        next.epoch = self.map().epoch + 1;
        let epoch = next.epoch;
        match raft.propose(Decision::Ranges(next)) {
            Some(_) => Ok(epoch),
            None => Err(ProposeError::NotLeader { leader: raft.leader() }),
        }
    }

    /// Proposes a change to who is in the cluster.
    ///
    /// **One node at a time.** Adding or removing a single member keeps the old majority and
    /// the new one overlapping, which is what makes the change safe without joint consensus -
    /// two at once can split into two disjoint majorities that each elect a leader. The
    /// "everything before it has committed" rule below is what enforces the one-at-a-time part.
    pub fn propose_members(&self, next: Vec<Member>) -> core::result::Result<(), ProposeError> {
        let mut raft = self.raft.lock().expect("no panic holds this lock");
        if !raft.is_leader() {
            return Err(ProposeError::NotLeader { leader: raft.leader() });
        }
        if raft.commit_index() != raft.last_index() {
            return Err(ProposeError::Busy);
        }
        let before = raft.membership().to_vec();
        if differences(&before, &next) > 1 {
            return Err(ProposeError::TooManyAtOnce);
        }
        match raft.propose(Decision::Members(next)) {
            Some(_) => Ok(()),
            None => Err(ProposeError::NotLeader { leader: raft.leader() }),
        }
    }

    /// Records that a copy has caught up, after a repair has made it true.
    ///
    /// Proposed rather than applied: whether a copy may be promoted is a fact every node has
    /// to agree on, and this is the one thing that makes a copy promotable again.
    pub fn mark_repaired(&self, node: NodeId) -> bool {
        let mut raft = self.raft.lock().expect("no panic holds this lock");
        if !raft.is_leader() {
            return false;
        }
        let mut next = self.map();
        if !next.is_stale(node) {
            return true;
        }
        next.stale.retain(|n| *n != node);
        next.epoch += 1;
        raft.propose(Decision::Ranges(next)).is_some()
    }
}

fn merge(into: &mut raft::Output, from: raft::Output) {
    into.persist |= from.persist;
    into.send.extend(from.send);
    into.applied.extend(from.applied);
}

/// The most recent moment this node could prove it was in touch with the agreement.
///
/// A follower: when its leader last spoke. A leader: the moment by which a majority had
/// answered, which is the only thing that distinguishes a leader from a node that used to be
/// one. A candidate has no lease at all, which is correct - it does not know who leads.
fn lease_anchor(raft: &Raft, now: u64) -> Option<u64> {
    if raft.is_leader() {
        // **Over the voters, and by their ids.** This used to walk `0..members()` as though the
        // member set were a dense range - true while membership was a file read once, and
        // wrong the moment a node can leave and keep its slot. A learner is excluded for the
        // same reason it does not count towards a majority: it cannot help prove this node is
        // still the leader.
        let voters = raft.voters();
        let mut heard: Vec<u64> = voters
            .iter()
            .filter_map(|n| if *n == raft.id() { Some(now) } else { raft.last_heard(*n) })
            .collect();
        heard.sort_unstable_by(|a, b| b.cmp(a));
        // The majority-th most recent. With three nodes that is the second: this node and one
        // other, which is a majority that has answered within that time.
        return heard.get(voters.len() / 2).copied();
    }
    let leader = raft.leader()?;
    if leader == raft.id() {
        return Some(now);
    }
    raft.last_heard(leader)
}

/// Why a change to the map was not proposed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProposeError {
    /// This node is not the one that decides. The leader, when it knows who that is.
    NotLeader {
        leader: Option<NodeId>,
    },
    /// Something is already in flight. Deciding about a state that has not settled is how two
    /// changes are made about one map and only one of them survives.
    Busy,
    /// More than one member added, removed or changed at once. Two at a time can split the
    /// cluster into two majorities that do not overlap, and each would elect its own leader.
    TooManyAtOnce,
    Invalid(raft::MapError),
}

impl core::fmt::Display for ProposeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotLeader { leader: Some(n) } => {
                write!(f, "this node does not decide the map; node {n} leads the agreement")
            }
            Self::NotLeader { leader: None } => {
                write!(f, "this node does not decide the map, and no leader is known yet")
            }
            Self::Busy => write!(
                f,
                "a change to the map is already in flight; wait for it to commit and try again"
            ),
            Self::TooManyAtOnce => write!(
                f,
                "a membership change moves one node at a time; two at once can leave two \
                 majorities that do not overlap, and each would elect its own leader"
            ),
            Self::Invalid(e) => write!(f, "the change would leave the map invalid: {e}"),
        }
    }
}

/// How many slots differ between two member lists, counting a longer list as that many more.
fn differences(before: &[Member], after: &[Member]) -> usize {
    let common = before.len().min(after.len());
    let changed = (0..common).filter(|i| before[*i] != after[*i]).count();
    changed + before.len().abs_diff(after.len())
}
