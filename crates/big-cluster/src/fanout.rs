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

//! One body, one node per slot, and the answers back in slot order.
//!
//! The bottom of every other module here: `Cluster::query`, a write's share and a repair's
//! pull all reach a peer through `Cluster::ask`. Answers are sorted by slot rather than left
//! in arrival order, which is what makes two runs of one query produce the same bytes.

use super::*;

impl<P: PagerMut + Sync> Cluster<P> {
    /// Decoding, with the node that sent the bytes named in the failure.
    pub(super) fn read<T>(
        &self,
        node: usize,
        decode: impl FnOnce() -> wire::Result<T>,
    ) -> Result<T> {
        decode().map_err(|why| ClusterError::Wire { node: self.describe(node), why })
    }

    // -----------------------------------------------------------------------------------
    // The fan-out itself
    // -----------------------------------------------------------------------------------

    /// Sends **one body per slot** to one node per slot, and collects the answers.
    ///
    /// A slot is a range, and it carries its candidates **in preference order**: the node
    /// serving it, then - in a cluster that has chosen availability - the other copies. The
    /// first that answers is the answer. A cluster that has chosen consistency passes one
    /// candidate per slot, so this is the same code doing less.
    ///
    /// **One body per slot rather than one shared body**, because a routed request has to say
    /// which shards it is for. A node may hold more than one range, and asking it twice with a
    /// body that did not name one would have it answer with everything both times - a `Count`
    /// that is silently double, which nothing downstream could contradict.
    ///
    /// Peers run concurrently, one thread each - there are as many as there are ranges, which
    /// is a number an operator wrote in a file - and this node's share runs on whichever
    /// thread reaches it. Answers come back in slot order regardless of arrival order, so a
    /// merge that is order-independent is also *deterministic*, which is what makes two runs
    /// of one query comparable.
    pub(super) fn fan_out_over<T: Send>(
        &self,
        slots: &[Vec<usize>],
        budget: Option<Duration>,
        path: &str,
        body: impl Fn(usize) -> Vec<u8> + Sync,
        decode: impl Fn(&[u8]) -> wire::Result<T> + Sync,
        local: impl Fn(usize) -> Result<T> + Sync,
    ) -> Result<Vec<(usize, T)>> {
        let started = Instant::now();
        let this = self.config.this_index();

        let mut answers: Vec<(usize, T)> = Vec::with_capacity(slots.len());
        let mut failure: Option<ClusterError> = None;

        std::thread::scope(|scope| {
            let handles: Vec<_> = slots
                .iter()
                .enumerate()
                .map(|(slot, candidates)| {
                    let decode = &decode;
                    let local = &local;
                    // Built once per slot rather than once per candidate: every copy of a range
                    // is asked the same question, and only one of them is asked at all unless
                    // the first cannot be reached.
                    let body = body(slot);
                    (
                        slot,
                        scope.spawn(move || {
                            let mut last = None;
                            for &node in candidates {
                                let attempt = if node == this {
                                    local(slot)
                                } else {
                                    // What is left of the budget when this request goes out,
                                    // rather than a fresh copy of it: a fan-out is one
                                    // deadline shared by every leg, not one deadline each.
                                    let left = budget.map(|b| b.saturating_sub(started.elapsed()));
                                    self.ask(node, path, &body, left).and_then(|bytes| {
                                        decode(&bytes).map_err(|why| ClusterError::Wire {
                                            node: self.describe(node),
                                            why,
                                        })
                                    })
                                };
                                match attempt {
                                    Ok(v) => return Ok(v),
                                    // Only a copy that could not be reached is worth trying
                                    // another copy for. A refusal is an answer: asking a
                                    // different node the same refused question would turn a
                                    // clear failure into a confusing one.
                                    Err(e) if e.is_unreachable() => last = Some(e),
                                    Err(e) => return Err(e),
                                }
                            }
                            Err(last.expect("a slot always has at least one candidate"))
                        }),
                    )
                })
                .collect();

            for (slot, handle) in handles {
                // Every slot is joined even once one has failed. They are already running, and
                // abandoning a thread would leave a socket open behind a response that has
                // already gone out.
                let joined = handle.join().unwrap_or_else(|_| {
                    Err(ClusterError::Unreachable {
                        node: format!("range {slot}"),
                        shards: String::new(),
                        why: "the request thread panicked".to_string(),
                    })
                });
                match joined {
                    Ok(v) => answers.push((slot, v)),
                    // The first failure is the one reported. A query that could not reach one
                    // range has failed whole, so the others are of no further interest.
                    Err(e) => failure = failure.take().or(Some(e)),
                }
            }
        });

        match failure {
            Some(e) => Err(e),
            None => {
                // Arrival order is not answer order. Sorting here is what makes two runs of
                // one query produce the same bytes, which is the difference between a result a
                // client can compare and one it cannot.
                answers.sort_by_key(|(slot, _)| *slot);
                Ok(answers)
            }
        }
    }

    /// One request to one peer, with every failure named the way an operator needs it.
    pub(super) fn ask(
        &self,
        i: usize,
        path: &str,
        body: &[u8],
        budget: Option<Duration>,
    ) -> Result<Vec<u8>> {
        // **A lookup rather than an index.** A node that joined while this one was running is
        // in the agreement and not in the cluster file this process read, so indexing the file
        // by a `NodeId` from the map is a panic on the request path.
        let name = self.name_of(i).unwrap_or_else(|| self.name_of_agreed(i));
        let shards = self.config.nodes().get(i).map(|n| n.shards.to_string()).unwrap_or_default();
        self.counters.sent();
        match self.peers.post(i, path, body, budget, repeatable(path)) {
            Ok(r) if r.is_ok() => Ok(r.body),
            Ok(r) => Err(self.counters.refused(ClusterError::Peer {
                node: name,
                status: r.status,
                code: r.code().unwrap_or_else(|| "unknown".to_string()),
                message: r.message().unwrap_or_else(|| "no message".to_string()),
            })),
            Err(ClientError::Timeout) => Err(self.counters.unreachable(ClusterError::Timeout)),
            Err(e) => Err(self.counters.unreachable(ClusterError::Unreachable {
                node: name,
                shards,
                why: e.to_string(),
            })),
        }
    }
}

/// Whether a request may be sent again after a reused connection failed with no answer.
///
/// A read is: asking twice is the same question. Interning is, because a row id is assigned
/// once and returned forever after. Writing is not - a `delete` sent twice reports how many
/// records the *second* one removed - and neither is a schema change, which would refuse its
/// own first attempt. Anything not named here is treated as a write, which is the answer that
/// is wrong in the cheap direction.
fn repeatable(path: &str) -> Repeatable {
    match path {
        path::QUERY | path::RECORDS | path::INTERN => Repeatable::Yes,
        _ => Repeatable::No,
    }
}
