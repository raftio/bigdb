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

//! The thread that does a node's slow, optional work on its own behalf.
//!
//! **Not the agreement's thread.** `Controller::run` ticks every fifty milliseconds with the
//! raft lock held, and everything it does must be over before the next heartbeat is due. A
//! balancing step is the opposite kind of work: it copies a range over the network, proposes to
//! the agreement and waits for the proposal to land - seconds, with the raft lock taken and
//! released along the way. Run from inside the driver it would deadlock against itself; run
//! from a worker it would hold a request's thread for the life of a move. So it has a thread of
//! its own, owned by the server, and the server stops it the way it stops its workers.
//!
//! **Every job here is off until something turns it on.** A node that reshapes its cluster, or
//! its file, without being asked is a node whose behaviour an operator cannot predict. The
//! jobs read their switches from the configuration on every pass, do nothing while the switch
//! is off, and cost one clock read a second for the privilege.

use crate::State;
use big_cluster::raft::{MoveState, RangeId};
use big_pager::PagerMut;
use big_wire::log;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How often the jobs are considered.
///
/// A second, not the agreement's fifty milliseconds: a pass that gets past its gates asks every
/// member what it weighs, and a pass that acts costs seconds. Nothing here is urgent.
const PERIOD: Duration = Duration::from_secs(1);

/// How often the sleep looks up to see whether the server is standing down.
///
/// A stop is a stop: a thread that sleeps a whole period before noticing is a shutdown that
/// takes a second longer than it should, and a test that waits on one.
const WAKE: Duration = Duration::from_millis(50);

/// After a job fails, how long before it is tried again - and the ceiling on that.
///
/// A cluster that cannot balance because a peer is down is not helped by being asked again
/// every second, and the peer is not helped by a request a second from every leader it has had.
const BACKOFF_FROM: Duration = Duration::from_secs(1);
const BACKOFF_TO: Duration = Duration::from_secs(60);

/// How long a range may sit in one state of a move before somebody is told.
///
/// A move takes seconds. Ten minutes in the same state is not a slow move; it is a move whose
/// driver died, and a range marked moving silences the balancer cluster-wide until an operator
/// runs `POST /admin/cluster/cancel`. Said once per transition rather than every pass, and
/// never acted on: a canceller racing a slow-but-progressing move would throw away real work.
const STUCK_AFTER: Duration = Duration::from_secs(600);

/// How much of the file has to be reclaimable before the tail is worth trying to give back.
///
/// A quarter. Below that the holes are what copy-on-write always leaves and what the freelist
/// is there to reuse, and taking the write lock to hand back a few pages would be paying for
/// nothing.
const RECLAIM_AT_PERCENT: u64 = 25;

/// And a floor, so that a database of a few hundred pages is left alone whatever the ratio: a
/// quarter of nothing is nothing, and an empty file has no tail worth trimming.
const RECLAIM_FLOOR_PAGES: u64 = 4_096;

/// Runs until the server stands down.
pub(crate) fn run<P: PagerMut + Sync + Send + 'static>(state: &State<P>) {
    let mut backoff = Backoff::default();
    let mut watch = Watch::default();
    while !state.stopping.load(Ordering::Relaxed) {
        if let Some((range, in_state, for_)) = watch.observe(moving(state), Instant::now()) {
            log::emit(
                log::Level::Warn,
                "move_stuck",
                &[
                    ("range", log::F::N(range)),
                    ("state", log::F::S(&format!("{in_state:?}"))),
                    ("for_secs", log::F::N(for_.as_secs())),
                ],
            );
        }

        // The handover first: while it is open nobody assigns row ids, and nothing the
        // balancer could do is worth more than closing it.
        let handed = match handover(state) {
            Ok(None) => Ok(()),
            Ok(Some(to)) => {
                log::emit(log::Level::Info, "schema_handover", &[("to", log::F::S(&to))]);
                Ok(())
            }
            Err(e) => {
                // A survivor that did not answer is retried; a survivor that contradicts
                // another is not something a retry can fix, and it is said at the level it
                // deserves. Both back off: the cluster is not helped by being asked again
                // every second.
                let level = if e.is_unreachable() { log::Level::Warn } else { log::Level::Error };
                log::emit(level, "schema_handover_failed", &[("error", log::F::S(&e.to_string()))]);
                Err(())
            }
        };

        // Local, and gated on nothing but its own switch: it asks no peer and proposes
        // nothing, so it runs on every node - clustered or not, leader or not.
        reclaim(state);

        let wait = match balance(state) {
            Ok(None) if handed.is_ok() => PERIOD,
            Ok(None) => backoff.next(),
            Ok(Some(did)) => {
                log::emit(log::Level::Info, "balance_step", &[("did", log::F::S(&did.describe()))]);
                PERIOD
            }
            Err(e) => {
                let wait = backoff.next();
                log::emit(
                    log::Level::Warn,
                    "balance_error",
                    &[
                        ("error", log::F::S(&e.to_string())),
                        ("retry_in_ms", log::F::N(wait.as_millis() as u64)),
                    ],
                );
                wait
            }
        };
        if wait <= PERIOD {
            backoff.reset();
        }
        sleep_unless_stopping(state, wait);
    }
}

/// One balancing step, if this node is the one that decides and the policy allows it.
///
/// **Only the agreement's leader**, and only when everything it has proposed has landed - the
/// same two gates `promotion` stands behind, for the same reason: a decision about a map that
/// has not settled is a decision about a state that is already gone. A node with no agreement
/// has nothing to balance.
fn balance<P: PagerMut + Sync + Send + 'static>(
    state: &State<P>,
) -> big_cluster::Result<Option<big_cluster::Balanced>> {
    let policy = &state.config.balance;
    if !policy.enabled {
        return Ok(None);
    }
    let Some(controller) = state.cluster.controller() else {
        return Ok(None);
    };
    if !controller.is_leader() || !controller.settled() {
        return Ok(None);
    }
    state.cluster.rebalance(policy)
}

/// Finishes a handover of the row-key namespace the agreement decided, if one is open.
///
/// **The agreement's leader, and only it.** It decided the successor, it is the one node that
/// can mark the successor ready, and it reaches every survivor. The successor itself does
/// nothing here: it is given the keys, and told when it holds them all.
fn handover<P: PagerMut + Sync + Send + 'static>(
    state: &State<P>,
) -> big_cluster::Result<Option<String>> {
    let Some(controller) = state.cluster.controller() else {
        return Ok(None);
    };
    if !controller.is_leader() || !controller.settled() {
        return Ok(None);
    }
    state.cluster.finish_schema_handover()
}

/// Gives trailing free pages back to the filesystem, when there are enough to be worth it.
///
/// **The only thing that makes a served database smaller.** Copy-on-write leaves holes, the
/// freelist reuses them lowest-first so the tail drains, and this hands the drained tail back.
/// `big compact` does the same job offline and wants the exclusive lock, which means stopping
/// the node.
///
/// Three things keep it out of the way. It does nothing until a quarter of the file is
/// reclaimable, so an ordinary database is never touched. It does nothing while a backup is
/// walking the file, because a backup holds a reader for its whole run: the horizon cannot
/// move, so there would be nothing to release and the attempt would only take the write lock.
/// And `ReadersActive` - a read transaction open anywhere in this process - is an ordinary
/// outcome rather than a failure, counted and retried on the next pass rather than logged.
fn reclaim<P: PagerMut + Sync + Send + 'static>(state: &State<P>) {
    if !state.config.reclaim {
        return;
    }
    if state.backing_up.load(Ordering::Relaxed) {
        return;
    }
    let m = state.cluster.local().metrics();
    if m.page_count < RECLAIM_FLOOR_PAGES
        || m.free_pages_reusable * 100 < m.page_count * RECLAIM_AT_PERCENT
    {
        return;
    }
    match state.cluster.local().reclaim() {
        Ok(0) => {}
        Ok(pages) => {
            state.metrics.pages_reclaimed(pages);
            log::emit(log::Level::Info, "reclaimed", &[("pages", log::F::N(pages))]);
        }
        // Not a failure: it means a query was in flight, which on a busy node is most of the
        // time. The counter is what says whether "most" has become "always".
        Err(_) => state.metrics.reclaim_blocked(),
    }
}

/// The one move in flight, if there is one. The map allows at most one.
fn moving<P: PagerMut + Sync>(state: &State<P>) -> Option<(RangeId, MoveState)> {
    state.cluster.map().ranges.iter().find_map(|r| r.moving.as_ref().map(|m| (r.id, m.state)))
}

/// Sleeps for `wait`, waking early if the server stands down meanwhile.
fn sleep_unless_stopping<P: PagerMut>(state: &State<P>, wait: Duration) {
    let mut left = wait;
    while !left.is_zero() && !state.stopping.load(Ordering::Relaxed) {
        let step = left.min(WAKE);
        std::thread::sleep(step);
        left -= step;
    }
}

/// Doubling, capped, reset on success. Local to the thread: nothing about a retry belongs in
/// the map.
#[derive(Default)]
struct Backoff {
    current: Option<Duration>,
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let next = self.current.map_or(BACKOFF_FROM, |c| (c * 2).min(BACKOFF_TO));
        self.current = Some(next);
        next
    }

    fn reset(&mut self) {
        self.current = None;
    }
}

/// Watches the one move in flight and says when it has been in one state for too long.
///
/// **Local, and deliberately so.** The obvious place for "since when" is the map, and it would
/// be wrong there twice: the balancer's determinism rests on reading no clock, and a timestamp
/// in a replicated value is a clock every node reads differently. So the steward remembers
/// what it saw and when, and forgets both the moment the move changes state or ends.
#[derive(Default)]
struct Watch {
    seen: Option<(RangeId, MoveState, Instant)>,
    warned: bool,
}

impl Watch {
    /// Feeds one observation. `Some` exactly once per stuck state: the moment it has lasted
    /// past the threshold, and not again until the move changes.
    fn observe(
        &mut self,
        now_moving: Option<(RangeId, MoveState)>,
        now: Instant,
    ) -> Option<(RangeId, MoveState, Duration)> {
        match (now_moving, self.seen) {
            (None, _) => {
                self.seen = None;
                self.warned = false;
                None
            }
            (Some((id, st)), Some((seen_id, seen_st, since))) if seen_id == id && seen_st == st => {
                let for_ = now.saturating_duration_since(since);
                if for_ >= STUCK_AFTER && !self.warned {
                    self.warned = true;
                    Some((id, st, for_))
                } else {
                    None
                }
            }
            (Some((id, st)), _) => {
                self.seen = Some((id, st, now));
                self.warned = false;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_a_ceiling_and_resets() {
        let mut b = Backoff::default();
        assert_eq!(b.next(), Duration::from_secs(1));
        assert_eq!(b.next(), Duration::from_secs(2));
        assert_eq!(b.next(), Duration::from_secs(4));
        for _ in 0..10 {
            b.next();
        }
        assert_eq!(b.next(), BACKOFF_TO, "capped, however many times it fails");
        b.reset();
        assert_eq!(b.next(), Duration::from_secs(1), "a success starts the clock again");
    }

    /// A move that progresses is never reported; one that does not is reported once, and
    /// again only if it stalls afresh in another state.
    #[test]
    fn a_stalled_move_is_reported_once_per_state() {
        let mut w = Watch::default();
        let t0 = Instant::now();
        let seeding = Some((7, MoveState::Seeding));
        let cutover = Some((7, MoveState::Cutover));

        assert_eq!(w.observe(seeding, t0), None, "first sight starts the clock");
        assert_eq!(w.observe(seeding, t0 + STUCK_AFTER / 2), None, "half way is not stuck");
        let stuck = w.observe(seeding, t0 + STUCK_AFTER);
        assert_eq!(stuck.map(|(id, st, _)| (id, st)), Some((7, MoveState::Seeding)));
        assert_eq!(w.observe(seeding, t0 + STUCK_AFTER * 2), None, "said once, not every pass");

        // It moved on. The clock restarts with the new state, and warns again only if that
        // one stalls too.
        assert_eq!(w.observe(cutover, t0 + STUCK_AFTER * 2), None);
        assert_eq!(w.observe(cutover, t0 + STUCK_AFTER * 2 + STUCK_AFTER / 2), None);
        assert!(w.observe(cutover, t0 + STUCK_AFTER * 3).is_some());

        // Finished: nothing to watch, nothing remembered.
        assert_eq!(w.observe(None, t0 + STUCK_AFTER * 4), None);
        assert_eq!(w.observe(seeding, t0 + STUCK_AFTER * 4), None, "a new move starts fresh");
    }
}
