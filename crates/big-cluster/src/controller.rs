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
use crate::raft::{self, Decision, Message, NodeId, Ownership, Raft, Store, Timing};
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
    /// Which node serves each range, as last committed. Read on the path of every request, so
    /// it is a lock held for a pointer's worth of time and never across any I/O.
    ownership: Arc<RwLock<Ownership>>,
    /// Every node holding each range, from the config file. Static: a promotion moves the
    /// answer within a group, never between groups, because no data moves.
    groups: Vec<Vec<NodeId>>,
    /// The range this node holds, if it holds one. A node is in exactly one group, which is
    /// what makes the lease a property of the node rather than of each request.
    mine: Option<usize>,
    /// Whether this node's range has a copy at all. A range of one cannot be taken away, so it
    /// is not fenced: fencing it would stop a node serving because a *different* machine is
    /// unreachable, which is an outage invented rather than avoided.
    fenced: bool,
    /// Milliseconds since this process started, at the last moment this node could prove it
    /// was still in touch with a majority. The lease, in one number.
    lease_at: AtomicU64,
    started: Instant,
    inbox: mpsc::Sender<Message>,
    outbox: Vec<Option<mpsc::SyncSender<Message>>>,
    stop: Arc<AtomicBool>,
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
    ) -> std::io::Result<Arc<Self>> {
        let started = Instant::now();
        let members: Vec<NodeId> = (0..config.nodes().len()).collect();
        let mut raft = Raft::new(config.this_index(), members, timing, 0);
        if let Some((term, voted_for, log)) = store.load()? {
            raft.restore(term, voted_for, log);
        }

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        // One sender thread per peer, each with a short queue. A heartbeat that cannot be
        // queued is dropped rather than waited for: this protocol was designed for a network
        // that loses messages, and blocking the one thread that drives it in order to reach a
        // node that is not answering is how a live cluster is brought down by a dead node.
        let mut outbox = Vec::with_capacity(peers.len());
        for i in 0..peers.len() {
            match i == config.this_index() {
                true => outbox.push(None),
                false => {
                    let (send, recv) = mpsc::sync_channel::<Message>(64);
                    outbox.push(Some(send));
                    let peers = Arc::clone(&peers);
                    let stop = Arc::clone(&stop);
                    let budget = timing.heartbeat;
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
                                    // A message that arrives twice decides nothing twice: a
                                    // term and an index say what a message means, so a retry
                                    // on a connection that was closed underneath us is free.
                                    Repeatable::Yes,
                                );
                            }
                        })
                        .expect("a thread per peer");
                }
            }
        }

        let ownership = Arc::new(RwLock::new(config.initial_ownership()));
        let mine = config.range_of_node(config.this_index());
        // A range with no copy is never fenced: nothing could take it away, so a node that
        // stops hearing from the agreement has lost nothing, and stopping would be an outage
        // invented rather than avoided.
        let fenced = mine.is_some_and(|r| config.group(r).len() > 1);

        let controller = Arc::new(Self {
            raft: Mutex::new(raft),
            store,
            leases,
            ownership,
            groups: (0..config.range_count()).map(|r| config.group(r).to_vec()).collect(),
            mine,
            fenced,
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

    /// Which node serves each range, as last committed.
    pub fn ownership(&self) -> Ownership {
        self.ownership.read().expect("no panic holds this lock").clone()
    }

    /// Whether this node may answer as the primary of its range right now.
    ///
    /// `true` for a range nothing could take away. For a replicated one it is the lease: this
    /// node stopped hearing from the agreement, so it has to assume it may already have been
    /// replaced, and answering would be the two-primaries failure this is all built to avoid.
    /// The range this node holds, primary or copy.
    pub fn range(&self) -> Option<usize> {
        self.mine
    }

    pub fn may_serve(&self) -> bool {
        if !self.fenced {
            return true;
        }
        self.since_lease() < self.leases.serve_for.as_millis() as u64
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

                let state =
                    out.persist.then(|| (raft.term(), raft.voted_for(), raft.log().to_vec()));
                (out, state)
            };

            // **Before anything is sent.** A vote that reaches the network and not the disk is
            // a vote this node can cast again after a restart, which is two leaders in one
            // term. The lock is released first because the disk is slow and the protocol is
            // not; nothing else writes this state, so there is nothing to race with.
            if let Some((term, voted_for, log)) = persist_state {
                if let Err(e) = self.store.save(term, voted_for, &log) {
                    // A node that cannot persist may not participate. Dropping the messages is
                    // what makes that true: it looks exactly like a node that is down, which
                    // is the one failure every other node here already handles.
                    crate::log_persist_failure(&e);
                    continue;
                }
            }

            for decision in out.applied {
                if let Decision::Own(o) = decision {
                    *self.ownership.write().expect("no panic holds this lock") = o;
                }
            }

            for (to, m) in out.send {
                if let Some(Some(sender)) = self.outbox.get(to) {
                    // Full means this peer is not keeping up, and a queued heartbeat that is
                    // already stale helps nobody.
                    let _ = sender.try_send(m);
                }
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

        let current = self.ownership();
        let mut next = current.clone();
        let mut changed = false;

        // Marked behind, and never unmarked here: only a repair knows whether a copy has
        // caught up, and the agreement is not the thing that would find out.
        //
        // Only inside a group that has more than one copy. A range held by one node cannot be
        // given to anybody, so marking its node behind would record a fact nothing acts on and
        // put a name in a report that has no repair to run.
        for group in self.groups.iter().filter(|g| g.len() > 1) {
            for &node in group {
                if !live(node) && !next.is_stale(node) {
                    next.stale.push(node);
                    changed = true;
                }
            }
        }
        next.stale.sort_unstable();

        for (range, group) in self.groups.iter().enumerate() {
            let Some(primary) = current.primary.get(range).copied() else { continue };
            if live(primary) {
                continue;
            }
            // A copy that is answering *and* has not missed a write. In group order, so two
            // leaders elected in sequence make the same choice and the range does not move
            // twice. When there is no such copy the range stays where it is and stays
            // unavailable, which is the honest answer: promoting a copy that is behind would
            // answer from data somebody else has and this one does not.
            let Some(&replacement) = group.iter().find(|n| live(**n) && !next.is_stale(**n)) else {
                continue;
            };
            next.primary[range] = replacement;
            changed = true;
        }

        // **A node that is serving keeps its mark.** It is tempting to clear one - the serving
        // copy is the truth by construction, so what could it be behind? - and it would be
        // wrong twice over. It prevents nothing: a mark is only ever read when *choosing* a
        // replacement, and the node being replaced is by then not a candidate. And it hides
        // the one case worth seeing, which is a node that came back with a replaced disk and
        // is now serving a range it no longer holds. `GET /verify` is how that is found; the
        // mark is what makes somebody look.

        changed.then_some(Decision::Own(next))
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
        let mut next = self.ownership();
        if !next.is_stale(node) {
            return true;
        }
        next.stale.retain(|n| *n != node);
        raft.propose(Decision::Own(next)).is_some()
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
        let mut heard: Vec<u64> = (0..raft.members())
            .filter_map(|n| if n == raft.id() { Some(now) } else { raft.last_heard(n) })
            .collect();
        heard.sort_unstable_by(|a, b| b.cmp(a));
        // The majority-th most recent. With three nodes that is the second: this node and one
        // other, which is a majority that has answered within that time.
        return heard.get(raft.members() / 2).copied();
    }
    let leader = raft.leader()?;
    if leader == raft.id() {
        return Some(now);
    }
    raft.last_heard(leader)
}
