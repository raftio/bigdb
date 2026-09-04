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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
    }
}

/// The schema leader's one job.
///
/// Answered by whichever node receives it, which is the leader because a coordinator sends it
/// nowhere else. There is no check here that this node *is* the leader: the config is what
/// decides that, every node reads the same file, and a check would turn a misconfiguration
/// that startup already refuses into a runtime error somewhere less useful.
pub(super) fn peer_intern<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let request = match wire::InternRequest::decode(&req.body) {
        Ok(r) => r,
        Err(e) => return unreadable(&e),
    };
    let keys: Vec<&str> = request.keys.iter().map(String::as_str).collect();
    match ctx.api().intern_keys(&request.table, &request.field, &keys) {
        Ok(rows) => {
            let mut out = Vec::new();
            wire::put_rows_ids(&mut out, &rows);
            Response::binary(out)
        }
        Err(e) => Response::from_error(&e),
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
    match ctx.cluster.allocate_here(&request.table, request.count) {
        Ok(from) => Response::binary(wire::put_u64_body(from)),
        Err(e) => super::from_cluster(&e),
    }
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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
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
        Err(e) => Response::from_error(&e),
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
