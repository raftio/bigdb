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

//! The clock behind an early answer.
//!
//! A write answered with `?ack=queued` is durable only once something commits it, and under any
//! load that something is the next writer along — a commit drains the whole queue, so a busy
//! node needs no help here at all. This thread exists for the node that is *not* busy: without
//! it, the last write before a lull would sit acknowledged and uncommitted until the next one
//! arrived, which could be minutes, and the promise "durable within `linger`" would be a
//! promise nobody keeps.
//!
//! **The engine owns the queue; this owns the clock.** `big-embed` is a library, and a library
//! that starts a thread behind its caller's back is a surprise in every process that embeds it.
//! So the buffer, the ceiling and the draining all live there, and what lives here is a thread
//! that calls `flush_pending` at the right moments and a `stop_writer` in the right place in
//! the shutdown order.
//!
//! **Not a job on the steward.** The steward ticks once a second and a pass that gets past its
//! gates can block for *seconds* copying a range across the network. Hanging a durability
//! window off the back of work like that is the wrong dependency: `linger` would mean "usually
//! two hundred milliseconds, unless a range is moving".
//!
//! **It sleeps on a condvar, not a timer.** `Api::await_pending` returns as soon as the first
//! acknowledged write lands, and otherwise waits out the timeout. So a node with this switched
//! on but nothing using it wakes on the timeout and finds an empty queue - and a node that is
//! not using early answers at all never starts this thread in the first place.

use crate::State;
use big_pager::PagerMut;
use big_wire::log;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The longest this will sleep before looking again, whatever `linger` says.
///
/// A stop has to be noticed promptly and `linger` is the caller's number, not ours: a
/// deployment that sets it to a minute should still shut down in well under a second.
const WAKE: Duration = Duration::from_millis(50);

/// Commits acknowledged work until the server stops.
pub(crate) fn run<P: PagerMut + Sync + Send + 'static>(state: &State<P>) {
    let linger = state.cluster.local().write_linger();
    while !state.stopping.load(Ordering::Relaxed) {
        // Returns early the moment work arrives, so `linger` is a ceiling on how long an
        // acknowledged write waits rather than a delay every one of them pays.
        if !state.cluster.local().await_pending(linger.min(WAKE)) {
            continue;
        }
        if state.stopping.load(Ordering::Relaxed) {
            break;
        }
        let carried = state.cluster.local().flush_pending();
        if carried > 0 {
            log::emit(log::Level::Debug, "flushed", &[("jobs", log::F::N(carried))]);
        }
    }
}

/// Commits what is left and reports it, for the end of `accept_loop`.
///
/// **Called after the workers are joined**, because only then can nothing else submit: a drain
/// that races a live worker leaves behind exactly the writes it exists to save.
///
/// A failure here is data that was acknowledged and did not land, which is the one thing an
/// early answer must not do quietly - so it is logged at `error` and never swallowed.
pub(crate) fn stop<P: PagerMut + Sync + Send + 'static>(state: &State<P>) {
    if !state.cluster.local().takes_async_writes() {
        return;
    }
    let carried = state.cluster.local().stop_writer();
    let left = state.cluster.local().group_stats();
    if left.async_failed > 0 {
        log::emit(
            log::Level::Error,
            "acknowledged writes did not land",
            &[("jobs", log::F::N(carried)), ("failed", log::F::N(left.async_failed))],
        );
    } else if carried > 0 {
        log::emit(log::Level::Info, "flushed on shutdown", &[("jobs", log::F::N(carried))]);
    }
}
