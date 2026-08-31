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

//! Liveness and readiness, the two routes that are never authenticated.
//!
//! Both are written against what a probe can act on. Neither reads data: a probe that gets
//! slower as the database grows eventually restarts a healthy process.

use super::*;

/// Liveness: can this process answer at all.
///
/// Touches nothing. A liveness probe that reads the database turns a slow query into a
/// restart, which is the one thing a liveness probe must never do.
pub(super) fn health() -> Response {
    Response::ok("{\"status\":\"ok\"}".to_string())
}

/// Readiness: can this process serve traffic right now.
///
/// Reads the catalog and the pager's own view of its file. That is deliberately more than
/// `/health` and deliberately less than a query: it proves the locks are not held and the
/// mapping is intact, without letting the answer depend on how much data there is.
///
/// **It does not check the peers.** A node that is ready is one that can serve its own shards;
/// whether another node is up changes nothing this one can do about it, and a readiness probe
/// that fails because a *different* machine is down takes a healthy node out of rotation for
/// somebody else's outage. The shards this node holds are reported so that a probe can see
/// which part of the space this process is answering for.
pub(super) fn ready<P: PagerMut + Sync>(ctx: &Ctx<'_, P>) -> Response {
    let m = ctx.api().metrics();
    let tables = ctx.cluster.schema().len();
    let node = ctx.cluster.config().this();
    // `serving` is the part a probe can act on: a node that has lost touch with the agreement
    // is refusing requests for its range, and a load balancer that keeps sending them is
    // sending them somewhere that will answer `503`. It is still *ready* - the engine is fine
    // and the node is one promotion away from serving again - so this is a field rather than a
    // failure.
    // The two versions an operator needs during a rolling upgrade, and they are not the same
    // question. `version` is which build this is; `wire` is what it speaks to other nodes, and
    // it only moves when a message changes shape - so two nodes with different `version` and
    // the same `wire` can run side by side, and two with different `wire` cannot.
    let build = format!(
        ",\"version\":{},\"wire\":{}",
        json::string(env!("CARGO_PKG_VERSION")),
        big_cluster::WIRE_VERSION
    );
    let agreement = match ctx.cluster.controller() {
        None => String::new(),
        Some(c) => {
            let behind: Vec<String> =
                ctx.cluster.behind().iter().map(|n| json::string(n)).collect();
            format!(
                ",\"serving\":{},\"term\":{},\"leader\":{},\"behind\":[{}]",
                ctx.cluster.may_serve(),
                c.term(),
                match c.leader() {
                    Some(i) => json::string(&ctx.cluster.config().nodes()[i].name),
                    None => "null".to_string(),
                },
                behind.join(",")
            )
        }
    };
    Response::ok(format!(
        "{{\"status\":\"ready\",\"tables\":{tables},\"txn_id\":{},\"pages\":{},\
         \"node\":{},\"shards\":{}{build}{agreement}}}",
        m.txn_id,
        m.page_count,
        json::string(&node.name),
        json::string(&node.shards.to_string())
    ))
}
