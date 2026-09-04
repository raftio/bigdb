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

//! What an operator can see about a node's place in the cluster.
//!
//! Three numbers matter more than the rest and they are the reason this exists. **How many
//! copies are behind**, because that is redundancy this cluster has lost and will not get back
//! until somebody repairs it. **Whether this node is serving**, because a node that has lost
//! touch with the agreement answers `503` for its own range and is otherwise perfectly healthy.
//! And **how often a peer could not be reached**, because a fan-out that fails intermittently
//! looks to a client like a database that is slow.
//!
//! Everything here is an atomic and a read of it, so `/metrics` never waits on a lock a request
//! needs.

use std::sync::atomic::{AtomicU64, Ordering};

/// Counters, incremented on the path they describe.
#[derive(Default, Debug)]
pub struct Counters {
    sent: AtomicU64,
    unreachable: AtomicU64,
    refused: AtomicU64,
    load_unanswered: AtomicU64,
    balance_moves: AtomicU64,
    balance_splits: AtomicU64,
    balance_admits: AtomicU64,
    balance_errors: AtomicU64,
    balance_drops_failed: AtomicU64,
    move_cancel_failed: AtomicU64,
}

impl Counters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
    }

    /// A peer that did not say what it weighs. To the balancer that node is neither a source
    /// nor a destination - so a count that climbs is a cluster that will not reshape, and
    /// without this number a transient timeout and a dead machine look the same.
    pub fn load_unanswered(&self) {
        self.load_unanswered.fetch_add(1, Ordering::Relaxed);
    }

    pub fn balance_move(&self) {
        self.balance_moves.fetch_add(1, Ordering::Relaxed);
    }

    pub fn balance_split(&self) {
        self.balance_splits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn balance_admit(&self) {
        self.balance_admits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn balance_error(&self) {
        self.balance_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// A move that landed, whose source could not let go of its copy. Space nobody reclaims
    /// until somebody looks; a background loop that only ever saw the move succeed would never
    /// tell them to.
    pub fn balance_drop_failed(&self) {
        self.balance_drops_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// A move that failed and could not be called off either. Each one is a range still marked
    /// moving, and a range marked moving silences the balancer cluster-wide.
    pub fn move_cancel_failed(&self) {
        self.move_cancel_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// A peer that did not answer. Returns the error it was given, so a call site can count and
    /// fail in one expression rather than in two lines that can drift apart.
    pub fn unreachable<E>(&self, e: E) -> E {
        self.unreachable.fetch_add(1, Ordering::Relaxed);
        e
    }

    /// A peer that answered and said no. Told apart from unreachable because an operator's next
    /// move is different: one is a machine to go and look at, the other is a disagreement
    /// between two builds or two configurations.
    pub fn refused<E>(&self, e: E) -> E {
        self.refused.fetch_add(1, Ordering::Relaxed);
        e
    }

    pub fn read(&self) -> Counts {
        Counts {
            sent: self.sent.load(Ordering::Relaxed),
            unreachable: self.unreachable.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            load_unanswered: self.load_unanswered.load(Ordering::Relaxed),
            balance_moves: self.balance_moves.load(Ordering::Relaxed),
            balance_splits: self.balance_splits.load(Ordering::Relaxed),
            balance_admits: self.balance_admits.load(Ordering::Relaxed),
            balance_errors: self.balance_errors.load(Ordering::Relaxed),
            balance_drops_failed: self.balance_drops_failed.load(Ordering::Relaxed),
            move_cancel_failed: self.move_cancel_failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Counts {
    /// Requests this node has made to another.
    pub sent: u64,
    /// How many of them found nobody there.
    pub unreachable: u64,
    /// How many were answered with a refusal.
    pub refused: u64,
    /// Peers that did not say what they weigh when the balancer asked.
    pub load_unanswered: u64,
    /// Ranges the balancer moved.
    pub balance_moves: u64,
    /// Tails the balancer cut for a node with nothing.
    pub balance_splits: u64,
    /// Learners the balancer made full members.
    pub balance_admits: u64,
    /// Balancing steps that failed.
    pub balance_errors: u64,
    /// Moves that landed but left their source holding a copy it could not drop.
    pub balance_drops_failed: u64,
    /// Moves that failed and could not be called off - ranges still marked moving.
    pub move_cancel_failed: u64,
}

/// Everything `/metrics` renders about the cluster, taken at one moment.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub nodes: usize,
    pub peers: usize,
    /// Whether any range has a copy. A cluster with none runs no agreement at all, and the
    /// three fields below are then constants rather than measurements.
    pub replicated: bool,
    /// Whether this node may currently answer for the range it holds.
    pub serving: bool,
    pub term: u64,
    pub leader: bool,
    /// Copies the agreement has marked behind. **Redundancy this cluster has lost.**
    pub behind: usize,
    /// Ranges with a move in flight. **Nonzero for longer than a move takes is the balancer
    /// silenced**: it plans nothing while anything is moving, and nothing clears the mark on
    /// its own.
    pub moving: usize,
    /// Whether the schema leader has finished taking the namespace over. `false` is a window
    /// in which nobody assigns row ids; one that stays `false` is a handover that is stuck.
    pub schema_ready: bool,
    /// Whether the last handover found two survivors disagreeing about a row id. **Somebody
    /// has to look**: nothing clears this but a handover that succeeds.
    pub handover_blocked: bool,
    pub counts: Counts,
}
