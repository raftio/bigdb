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

//! One address in front of many nodes.
//!
//! # What this is not
//!
//! **It does not route by key, and that is a decision rather than a gap.** Every node in a big
//! cluster is a coordinator: the one that receives a request plans it, fans it out to whichever
//! nodes hold the ranges involved, and merges what comes back. There is no dedicated coordinator
//! tier because a coordinator holds no state between requests. A proxy that computed
//! shard-to-node itself would be duplicating `big-cluster`'s `ownership.rs`, and would be wrong
//! for as long as it took a moved range to reach it — a wrongness the cluster has no way to
//! correct, because the proxy is not in the agreement.
//!
//! So the question this answers is not *which node owns this key*. It is *which node is alive*,
//! and that has a published answer: `GET /ready` is unauthenticated and reports `serving`, which
//! the daemon documents as the field a load balancer may act on.
//!
//! # What it is for
//!
//! A cluster today publishes one node's port, because publishing three would suggest a client
//! has to choose and it does not. But the clients hold one connection to one address with no
//! failover, so the published node is a single point of failure for machines that are all
//! perfectly healthy. This closes that gap: one address, several nodes behind it, and a request
//! that lands on a node which has stopped serving lands somewhere else instead.
//!
//! It carries a second job that is smaller to describe and just as hard to retrofit: the
//! [`allowlist`] names every route a client may reach through it, so the twenty-one `/internal/*`
//! peer routes are unreachable by construction rather than by a rule somebody has to remember.
//!
//! # What it holds
//!
//! Nothing. No file, no lock, no credential. A client's `Authorization: Basic` header is
//! forwarded untouched and is never parsed here — the daemon stays the only place a password is
//! verified, and this process never has one as a `String`. The proxy presents no client
//! certificate either, which is what keeps `/internal/*` closed at the connection as well as at
//! the route table.

pub mod allowlist;
pub mod config;
pub mod forward;
pub mod headers;
pub mod health;
pub mod listen;
pub mod metrics;
pub mod ops;
pub mod pool;
pub mod upstream;
