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
}

impl Counters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
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
    pub counts: Counts,
}
