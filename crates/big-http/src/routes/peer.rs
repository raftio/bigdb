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

//! The fan-out's other end
//!
//! Each of these does for one node what its public counterpart does for the cluster, against
//! this node's own database and nothing else. None of them fans out again: a coordinator sends
//! each owner only what that owner holds, so a peer that re-routed would be routing a request
//! that has already been routed.

use super::*;

/// A peer's plan, run here, over the shards this node owns.
///
/// The plan arrives resolved. It is not re-planned against this node's schema: two nodes
/// planning the same text is exactly the disagreement the wire format exists to prevent.
pub(super) fn peer_query<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    if let Some(refusal) = not_serving(ctx) {
        return refusal;
    }
    let request = match wire::QueryRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    let opts = QueryOptions {
        database: None,
        limits: None,
        // What is left of the coordinator's budget, not a fresh one. Memory ceilings are not
        // carried: they are this node's own property, and a node's ceiling is about the memory
        // it has rather than about the request that arrived.
        timeout: request.timeout_ms.map(Duration::from_millis).or(ctx.query_timeout),
        cancel: ctx.cancel.clone(),
        // **What this node answers for is what it was asked for**, not what is on its disk. A
        // node can hold more than one range, and one that has just handed a range away still
        // holds those fragments until it deletes them. `None` from a peer that predates the
        // scope would mean everything, which is why the coordinator always sends one.
        shards: request.shards,
    };
    match ctx.api().execute(&request.plan, &opts) {
        Ok(value) => Response::binary(wire::encode_value(&value)),
        Err(e) => crate::status::response_for(&e),
    }
}

pub(super) fn peer_records<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    if let Some(refusal) = not_serving(ctx) {
        return refusal;
    }
    let request = match wire::RecordsRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    // The coordinator asked for a page and will cut the merge to size itself; a limit that
    // does not fit this machine's `usize` is therefore harmless to clamp.
    let limit = request.limit.try_into().unwrap_or(usize::MAX);
    match ctx.api().records_in(&request.table, request.after, limit, request.shards) {
        Ok(ids) => {
            let mut out = Vec::new();
            wire::put_records(&mut out, &ids);
            Response::binary(out)
        }
        Err(e) => crate::status::response_for(&e),
    }
}

/// Facts, and what their keys mean.
///
/// The assignments and the facts land in one transaction, so a batch this node refuses leaves
/// neither behind. A mapping that contradicts one this node already holds is refused rather
/// than overwritten - that is the check that makes routing every key through one node worth
/// doing, applied at the node that would otherwise be the one to disagree.
pub(super) fn peer_import<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    if let Some(refusal) = not_serving(ctx) {
        return refusal;
    }
    let request = match wire::ImportRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    let keys: Vec<big_embed::KeyAssignment<'_>> = request
        .keys
        .iter()
        .map(|a| big_embed::KeyAssignment { field: &a.field, key: &a.key, row: a.row })
        .collect();
    // **Before anything lands.** The coordinator says which shards it believed this node owns;
    // a batch routed by a map that has since changed belongs somewhere else, and writing it
    // here would be a write no read ever finds.
    if let Err(e) = ctx.cluster.check_route(request.routed.as_ref()) {
        return super::from_cluster(&e);
    }
    let facts: Vec<big_embed::Fact<'_>> = request.facts.iter().map(OwnedFact::as_fact).collect();
    match ctx.api().import_with_keys(&request.table, &keys, &facts) {
        Ok(()) => Response::binary(wire::put_u64_body(facts.len() as u64)),
        Err(e) => crate::status::response_for(&e),
    }
}

pub(super) fn peer_delete<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    if let Some(refusal) = not_serving(ctx) {
        return refusal;
    }
    let request = match wire::DeleteRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    if let Err(e) = ctx.cluster.check_route(request.routed.as_ref()) {
        return super::from_cluster(&e);
    }
    match ctx.api().delete(&request.table, &request.records) {
        Ok(n) => Response::binary(wire::put_u64_body(n)),
        Err(e) => crate::status::response_for(&e),
    }
}

/// The schema leader's one job.
///
/// **Checked, not assumed.** It used to be assumed: the leader was a name in the config, every
/// node read the same file, and a request could only have been sent here on purpose. The
/// leader is a field in the map now, and a coordinator holding a map one decision old sends
/// this to whoever led *then* - which is how two nodes come to intern at once, the one failure
/// that hands one string two row ids and that nothing downstream can see. So the request says
/// who it thinks leads, and this node says whether it agrees before it assigns anything.
pub(super) fn peer_intern<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let request = match wire::InternRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    if let Err(e) = ctx.cluster.check_schema_lead(request.led.as_ref()) {
        return super::from_cluster(&e);
    }
    let keys: Vec<&str> = request.keys.iter().map(String::as_str).collect();
    match ctx.api().intern_keys(&request.table, &request.field, &keys) {
        Ok(rows) => {
            let mut out = Vec::new();
            wire::put_rows_ids(&mut out, &rows);
            Response::binary(out)
        }
        Err(e) => crate::status::response_for(&e),
    }
}

/// A run of record ids, which only the schema leader hands out.
///
/// The same shape as interning, and for the same reason: two coordinators deciding "one past
/// the highest" would decide the same number and write two records into one.
pub(super) fn peer_allocate<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let request = match wire::AllocateRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    if let Err(e) = ctx.cluster.check_schema_lead(request.led.as_ref()) {
        return super::from_cluster(&e);
    }
    match ctx.cluster.allocate_here(&request.table, request.count) {
        Ok(from) => Response::binary(wire::put_u64_body(from)),
        Err(e) => super::from_cluster(&e),
    }
}

/// `POST /internal/reserve`: raise the agreement's ceiling on record ids for a table.
///
/// Asked of the agreement's leader by a schema leader that is not it, once per block of ids.
/// The body is the same shape floors travel in - `(table, one past the highest)` - and the
/// answer is the epoch the ceiling landed at.
pub(super) fn peer_reserve<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let floors = match wire::get_floors(&req.body) {
        Ok(f) => f,
        Err(e) => return unreadable(&e),
    };
    let mut epoch = 0;
    for (table, upto) in &floors {
        match ctx.cluster.reserve_here(table, *upto) {
            Ok(at) => epoch = at,
            Err(e) => return super::from_cluster(&e),
        }
    }
    Response::binary(wire::put_u64_body(epoch))
}

/// `POST /internal/schema/step-down`: stop interning, the namespace is being handed over.
///
/// The body is the map epoch the move read. This node refuses to intern or allocate until the
/// map is past it - which, once the move lands, it is, and by then the map names somebody
/// else. Answered with the epoch, so the caller knows which one was heard.
pub(super) fn peer_schema_step_down<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
) -> Response {
    let epoch = match wire::get_u64_body(&req.body) {
        Ok(e) => e,
        Err(e) => return unreadable(&e),
    };
    ctx.cluster.stand_down_schema(epoch);
    Response::binary(wire::put_u64_body(epoch))
}

/// One past the highest record id this node holds, which is its share of the answer the leader
/// allocates above.
pub(super) fn peer_next_record<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let request = match wire::TableRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    match ctx.cluster.local_next_record(&request.table, request.shards) {
        Ok(next) => Response::binary(wire::put_u64_body(next)),
        Err(e) => super::from_cluster(&e),
    }
}

/// One schema change the leader has already ruled legal.
pub(super) fn peer_ddl<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let op = match wire::Ddl::decode(&req.body) {
        Ok(op) => op,
        Err(e) => return unreadable(&e),
    };
    match big_cluster::apply_ddl(ctx.api(), &op) {
        Ok(n) => Response::binary(wire::put_u64_body(n)),
        Err(e) => crate::status::response_for(&e),
    }
}

/// Everything this node holds, as one number, for a coordinator comparing it against the other
/// copies of the same range.
///
/// A scan, and deliberately not on any probe's path: nothing calls this except `GET /verify`,
/// which an operator runs on purpose.
pub(super) fn peer_digest<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    // An empty body is a digest of everything, which is what `GET /verify` asked for before a
    // node could hold two ranges. A body names the range being compared.
    let shards = match req.body.is_empty() {
        true => None,
        false => match wire::get_shards_body(&req.body) {
            Ok(s) => s,
            Err(e) => return unreadable(&e),
        },
    };
    match big_cluster::digest::digest_in(ctx.api(), shards) {
        Ok(d) => Response::binary(wire::put_u64_body(d)),
        Err(e) => crate::status::response_for(&e),
    }
}

/// One message of the agreement, handed to the thread that decides.
///
/// Answered immediately and with nothing: a reply is a new request in the other direction, not
/// the body of this response. That is what the protocol expects - a node answers a vote when it
/// has decided, not while a socket is still open - and it keeps a node that is thinking from
/// holding a worker on the node that asked.
pub(super) fn peer_raft<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let Some(controller) = ctx.cluster.controller() else {
        // Nothing here has a copy, so there is nothing to agree about. Said plainly rather
        // than ignored: a peer sending this has a different config file, which is worth
        // finding out from a `409` rather than from a range that quietly never fails over.
        return Response::failure(
            409,
            "no_agreement",
            "this node runs no agreement; no range in its cluster file has a copy",
        );
    };
    match wire::get_raft(&req.body) {
        Ok(m) => {
            controller.deliver(m);
            Response::binary(Vec::new())
        }
        Err(e) => unreadable(&e),
    }
}

/// Catches up every copy the agreement has marked behind.
///
/// A scan and a copy, run deliberately. It is the other half of letting a write stand when a
/// spare cannot be reached: without it, one blip costs a cluster its redundancy permanently,
/// because a copy marked behind is a copy that will never be promoted.
pub(super) fn repair<P: PagerMut + Sync>(ctx: &Ctx<'_, P>) -> Response {
    match ctx.cluster.repair() {
        Ok(reports) => Response::ok(json::repaired(&reports)),
        Err(e) => from_cluster(&e),
    }
}

// -------------------------------------------------------------------------------------------
// Reshaping the cluster
//
// Three verbs, and all four ways of asking for one - an operator at a terminal, a node joining
// itself, an autoscaler reading a metric, a controller reacting to a pod - come through them.
// Nothing here decides *when*; that belongs to whoever is asking.
// -------------------------------------------------------------------------------------------

/// What the cluster looks like right now.
pub(super) fn cluster_topology<P: PagerMut + Sync>(ctx: &Ctx<'_, P>) -> Response {
    Response::ok(json::topology(&ctx.cluster.topology()))
}

/// `POST /admin/cluster/split?at=<shard>&to=<node>`
///
/// **The scale-out that moves no bytes.** Cutting the open tail above everything written so far
/// and handing the upper half to an empty node costs one entry in the agreement and not one
/// byte on the wire. `to` is optional: without it the range is merely divided, which is what an
/// operator does before moving half of it somewhere.
pub(super) fn cluster_split<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let Some(at) = req.param("at").and_then(|v| v.parse::<u64>().ok()) else {
        return Response::failure(
            400,
            "bad_request",
            "split needs ?at=<shard>, the first shard of the new upper range",
        );
    };
    let to = req.param("to").map(|v| v.into_owned());
    match ctx.cluster.split_range(at, to.as_deref()) {
        Ok(id) => Response::ok(format!("{{\"range\":{id}}}")),
        Err(e) => from_cluster(&e),
    }
}

/// `POST /admin/cluster/merge?range=<id>` - joins a range to the one after it.
pub(super) fn cluster_merge<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let Some(id) = req.param("range").and_then(|v| v.parse::<u64>().ok()) else {
        return Response::failure(
            400,
            "bad_request",
            "merge needs ?range=<id>, the lower of the two ranges to join",
        );
    };
    match ctx.cluster.merge_range(id) {
        Ok(()) => Response::ok(format!("{{\"range\":{id}}}")),
        Err(e) => from_cluster(&e),
    }
}

/// `POST /admin/cluster/node?name=<name>&addr=<host:port>` - a node joins, as a learner.
///
/// **A learner, not a voter.** A node that has just arrived holds no range and has not caught
/// up on the log; counting it towards a majority would raise the bar for every election while
/// it contributed nothing to one. `admit` is the step that makes it a full member.
pub(super) fn cluster_add_node<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let (Some(name), Some(addr)) = (req.param("name"), req.param("addr")) else {
        return Response::failure(
            400,
            "bad_request",
            "adding a node needs ?name=<name>&addr=<host:port>",
        );
    };
    match ctx.cluster.add_node(&name, &addr) {
        Ok(()) => Response::ok(format!("{{\"node\":{}}}", json::string(&name))),
        Err(e) => from_cluster(&e),
    }
}

/// Which of the three one-node changes a request is.
pub(super) enum Membership {
    Admit,
    Drain,
    Remove,
}

/// `POST /admin/cluster/{admit,drain}` and `DELETE /admin/cluster/node`, all `?name=<node>`.
pub(super) fn cluster_member<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
    what: Membership,
) -> Response {
    let Some(name) = req.param("name") else {
        return Response::failure(400, "bad_request", "this needs ?name=<node>");
    };
    let done = match what {
        Membership::Admit => ctx.cluster.admit(&name),
        Membership::Drain => ctx.cluster.drain_node(&name),
        Membership::Remove => ctx.cluster.remove_node(&name),
    };
    match done {
        Ok(()) => Response::ok(format!("{{\"node\":{}}}", json::string(&name))),
        Err(e) => from_cluster(&e),
    }
}

/// `POST /admin/cluster/move?range=<id>&to=<node>` - hand a populated range over.
///
/// **A scan and a copy, run deliberately**, like `POST /repair`: it holds this request for as
/// long as the range takes to copy. Reads of the range never stop; writes to it are refused,
/// retryably, only for the last pass.
pub(super) fn cluster_move<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let (Some(range), Some(to)) = (req.param("range"), req.param("to")) else {
        return Response::failure(400, "bad_request", "a move needs ?range=<id>&to=<node>");
    };
    let Ok(range) = range.parse::<u64>() else {
        return Response::failure(400, "bad_request", "?range= takes a range id");
    };
    match ctx.cluster.move_range(range, &to) {
        Ok(report) => Response::ok(json::moved(&report)),
        Err(e) => from_cluster(&e),
    }
}

/// `POST /admin/cluster/replica?range=<id>&to=<node>` - one more copy of a range.
///
/// Long-running like a move, and for the same reason: it copies. Unlike a move it refuses
/// nothing while it runs - see [`big_cluster::Cluster::add_replica`].
pub(super) fn cluster_add_replica<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let (Some(range), Some(to)) = (req.param("range"), req.param("to")) else {
        return Response::failure(400, "bad_request", "a copy needs ?range=<id>&to=<node>");
    };
    let Ok(range) = range.parse::<u64>() else {
        return Response::failure(400, "bad_request", "?range= takes a range id");
    };
    match ctx.cluster.add_replica(range, &to) {
        Ok(report) => Response::ok(json::moved(&report)),
        Err(e) => from_cluster(&e),
    }
}

/// `DELETE /admin/cluster/replica?range=<id>&from=<node>` - one copy fewer.
///
/// The map stops naming it; nothing is deleted from the node itself.
pub(super) fn cluster_drop_replica<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
) -> Response {
    let (Some(range), Some(from)) = (req.param("range"), req.param("from")) else {
        return Response::failure(
            400,
            "bad_request",
            "dropping a copy needs ?range=<id>&from=<node>",
        );
    };
    let Ok(range) = range.parse::<u64>() else {
        return Response::failure(400, "bad_request", "?range= takes a range id");
    };
    match ctx.cluster.drop_replica(range, &from) {
        Ok(()) => Response::ok(format!("{{\"dropped\":{}}}", json::string(&from))),
        Err(e) => from_cluster(&e),
    }
}

/// `POST /admin/cluster/cancel?range=<id>` - abandon a move.
///
/// Nothing is ever read from the target of a move that has not completed, so this loses only
/// the copying already done.
pub(super) fn cluster_cancel<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let Some(range) = req.param("range").and_then(|v| v.parse::<u64>().ok()) else {
        return Response::failure(400, "bad_request", "cancelling needs ?range=<id>");
    };
    match ctx.cluster.cancel_move(range) {
        Ok(()) => Response::ok(format!("{{\"range\":{range}}}")),
        Err(e) => from_cluster(&e),
    }
}

/// `POST /admin/cluster/rebalance` - take one balancing step, if the facts call for one.
///
/// **One step per call.** A cluster that needs three moves takes three calls, each against
/// facts gathered afresh - because `each move is a moment where a query can fail`, and a plan
/// made before the first move is a plan about a cluster that no longer exists.
///
/// This is what an autoscaler or a Kubernetes controller calls on a timer. `?force=true` runs
/// the step even when the policy is switched off, which is what makes it usable as an
/// operator's command on a cluster that does not balance itself.
pub(super) fn cluster_rebalance<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let mut policy = ctx.balance;
    if req.param("force").is_some_and(|v| v == "true") {
        policy.enabled = true;
    }
    match ctx.cluster.rebalance(&policy) {
        Ok(None) => Response::ok("{\"did\":null}".to_string()),
        Ok(Some(what)) => Response::ok(format!("{{\"did\":{}}}", json::string(&what.describe()))),
        Err(e) => from_cluster(&e),
    }
}

/// `POST /admin/cluster/schema-leader?to=<node>` - hand the row-key namespace over.
///
/// **The one change that corrupts rather than fails**, so it is worth the wait: it copies every
/// row key of every table and the record ids the old leader has promised but not written, and
/// only then commits. See `Cluster::move_schema_leader`.
pub(super) fn cluster_schema_leader<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
) -> Response {
    let Some(to) = req.param("to") else {
        return Response::failure(400, "bad_request", "this needs ?to=<node>");
    };
    match ctx.cluster.move_schema_leader(&to) {
        Ok(()) => Response::ok(format!("{{\"schema_leader\":{}}}", json::string(&to))),
        Err(e) => from_cluster(&e),
    }
}

/// Every fragment of a table, with the count that stands in for its contents.
pub(super) fn peer_fragments<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let request = match wire::FragmentsRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    match ctx.api().fragments(&request.table) {
        Ok(list) => {
            let mut out = Vec::new();
            wire::put_fragment_list(&mut out, &list);
            Response::binary(out)
        }
        Err(e) => crate::status::response_for(&e),
    }
}

pub(super) fn peer_fragment<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let request = match wire::FragmentRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    match ctx.api().fragment(&request.addr) {
        Ok((meta, data)) => Response::binary(
            wire::FragmentBody { addr: request.addr, meta: meta.unwrap_or_default(), data }
                .encode(),
        ),
        Err(e) => crate::status::response_for(&e),
    }
}

/// Takes one fragment whole, replacing whatever was there.
///
/// Not a write in the ordinary sense and not authorised as one: this replaces what a node
/// holds rather than adding to it, which is why it needs `admin`.
pub(super) fn peer_fragment_put<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let body = match wire::FragmentBody::decode(&req.body) {
        Ok(b) => b,
        Err(e) => return unreadable(&e),
    };
    match ctx.api().replace_fragment(&body.addr, body.meta, &body.data) {
        Ok(()) => Response::binary(Vec::new()),
        Err(e) => crate::status::response_for(&e),
    }
}

pub(super) fn peer_keys<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let request = match wire::FragmentsRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    match ctx.api().row_keys(&request.table) {
        Ok(keys) => Response::binary(
            wire::KeysBody {
                table: request.table,
                keys: keys
                    .into_iter()
                    .map(|(field, key, row)| big_cluster::Assignment { field, key, row })
                    .collect(),
            }
            .encode(),
        ),
        Err(e) => crate::status::response_for(&e),
    }
}

pub(super) fn peer_keys_put<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let body = match wire::KeysBody::decode(&req.body) {
        Ok(b) => b,
        Err(e) => return unreadable(&e),
    };
    let keys: Vec<big_embed::KeyAssignment<'_>> = body
        .keys
        .iter()
        .map(|a| big_embed::KeyAssignment { field: &a.field, key: &a.key, row: a.row })
        .collect();
    match ctx.api().assign_keys(&body.table, &keys) {
        Ok(()) => Response::binary(Vec::new()),
        Err(e) => crate::status::response_for(&e),
    }
}

/// One copy has caught up, so stop refusing to promote it.
///
/// Answered by the agreement's leader, because it is the only node that can propose anything.
/// A node that is not the leader says so rather than pretending, since a mark that was never
/// cleared is a copy that silently stays unpromotable.
pub(super) fn peer_repaired<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let node = match wire::get_node(&req.body) {
        Ok(n) => n,
        Err(e) => return unreadable(&e),
    };
    let Some(controller) = ctx.cluster.controller() else {
        return Response::failure(409, "no_agreement", "this node runs no agreement");
    };
    if controller.mark_repaired(node) {
        Response::binary(Vec::new())
    } else {
        Response::failure(
            409,
            "not_the_leader",
            "only the agreement's leader can record that a copy has caught up",
        )
    }
}
