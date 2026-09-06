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

//! The queries running right now, so that one of them can be named and stopped.
//!
//! # Why the registry is here and not lower
//!
//! **The cancellation flag already exists, and this is where it is made.** `DbRead` carries an
//! `Arc<AtomicBool>` and checks it at every fragment; the server mints one per request and hands
//! it to the watchdog that trips it when the client's socket goes away. Everything needed to stop
//! a query has been in place the whole time - what was missing is an *address*. So this adds a
//! map from an id to the flag, at the layer that already holds the flag, rather than growing one
//! in `big-embed` that the layer above would have to reach down into.
//!
//! # What a kill stops, and what it does not
//!
//! Setting the flag stops the query on **this** node. A coordinator's in-flight legs to other
//! owners run to completion, and each of those is stopped by its own socket closing - which the
//! watchdog already does. A kill that reached every leg is a fan-out and a decision for later;
//! this one is honest about its reach, which is why the listing carries a `node` column.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One running query, as the listing sees it.
#[derive(Clone)]
pub struct Running {
    pub id: String,
    pub started: Instant,
    /// The role that sent it, or `None` for a trusted caller with no role to name.
    pub role: Option<String>,
    pub database: String,
    pub text: String,
}

/// Every query this node is running, addressable by id.
///
/// A `Mutex<BTreeMap>` and not something cleverer on purpose: it is touched **twice per
/// statement** - once to register, once to forget - and never per fragment or per record. A lock
/// on that path costs nothing measurable, and a lock-free structure here would be complexity
/// bought with no contention to spend it on.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<BTreeMap<String, (Arc<AtomicBool>, Running)>>,
    next: AtomicU64,
}

impl Registry {
    /// Registers a query and hands back a guard that forgets it.
    ///
    /// **A guard rather than a pair of calls**, because the un-registering has to happen on every
    /// path out of a request including a panic - and an entry left behind is a query that appears
    /// in the listing for ever and can be "killed" without effect.
    pub fn register(
        self: &Arc<Self>,
        node: &str,
        cancel: Arc<AtomicBool>,
        role: Option<String>,
        database: String,
        text: String,
    ) -> Guard {
        // `<node>/<counter>`, the shape the write path already stamps on `X-Big-Txn`. One format
        // for an operator to learn, and the prefix is what turns routing a kill into a lookup
        // rather than a broadcast.
        let id = format!("{node}/{}", self.next.fetch_add(1, Ordering::Relaxed));
        let running = Running { id: id.clone(), started: Instant::now(), role, database, text };
        if let Ok(mut map) = self.inner.lock() {
            map.insert(id.clone(), (cancel, running));
        }
        Guard { registry: Arc::clone(self), id }
    }

    /// Trips the flag of one query. `false` when no query is running under that id.
    ///
    /// A query that finished a moment ago is not an error to have named: the answer is that it
    /// is not running, which is what the caller wanted either way.
    pub fn kill(&self, id: &str) -> bool {
        let Ok(map) = self.inner.lock() else { return false };
        match map.get(id) {
            Some((cancel, _)) => {
                cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Every query running on this node, oldest first.
    pub fn running(&self) -> Vec<Running> {
        let Ok(map) = self.inner.lock() else { return Vec::new() };
        let mut out: Vec<Running> = map.values().map(|(_, r)| r.clone()).collect();
        out.sort_by_key(|r| r.started);
        out
    }

    fn forget(&self, id: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(id);
        }
    }
}

/// Forgets a registered query when the request ends, however it ends.
pub struct Guard {
    registry: Arc<Registry>,
    id: String,
}

impl Guard {
    /// The id this query answers to, for a client that wants to be able to kill its own.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.registry.forget(&self.id);
    }
}
