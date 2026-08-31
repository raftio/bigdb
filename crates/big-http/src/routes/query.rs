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

//! Reading and writing data, over the cluster.
//!
//! The routes with a budget: each of these can run long enough that the caller may be gone
//! before the answer is, so they carry the deadline, the cancellation flag and the memory
//! ceiling that let this node stop working on an answer nobody is waiting for.

use super::*;

pub(super) fn query<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request, table: &str) -> Response {
    let page = match page(req) {
        Ok(p) => p,
        Err(why) => return Response::failure(422, "bad_parameter", &why),
    };
    let text = match req.text() {
        Ok(t) => t.trim(),
        Err(e) => return e.into_response(),
    };
    // The only route that can run long, so the only one carrying a deadline and a flag. Both
    // are `None` unless the server was configured with them, which keeps the default behaviour
    // of a query exactly what it always was.
    let opts =
        QueryOptions { limits: None, timeout: ctx.query_timeout, cancel: ctx.cancel.clone() };
    match ctx.cluster.query(table, text, &opts) {
        // Refused rather than ignored. `Count(All())&limit=10` is a client that believes it is
        // paging and is not; answering it with an unpaged count would be answering a question
        // they did not ask. 422 because the URI is fine and the combination is not.
        Ok(value) if !page.is_default() && value.as_rows().is_none() => Response::failure(
            422,
            "not_pageable",
            "after and limit apply to a query that returns records; this one does not",
        ),
        Ok(value) => Response::ok(json::value_paged(&value, page)),
        // Classified rather than flattened: a query naming a table that is not there is a 404,
        // one the parser rejected is a 400, one that ran past its deadline is a 504, one an
        // owner could not answer is a 503 naming the shards it holds.
        Err(e) => from_cluster(&e),
    }
}

/// `POST /sql` - one `SELECT`, answered as a result set.
///
/// No query string: `LIMIT` is part of the statement, and a second way to say the same thing is
/// a second way for a client to contradict itself. The deadline, the cancellation flag and the
/// memory ceiling are the ones `/query` gets, because a statement that runs long is the same
/// problem whichever language it was written in.
pub(super) fn sql<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let text = match req.text() {
        Ok(t) => t.trim(),
        Err(e) => return e.into_response(),
    };
    // **A schema change over `/sql` needs `admin`, not the `read` this route is authorised
    // with.** The role check in `dispatch` runs before any body is decoded, which is what keeps
    // it cheap and is why it cannot know what statement arrived; the route's role is therefore a
    // floor, and a statement is allowed to raise it. Without this a read-only token could create
    // tables - the same power `POST /table/{t}` demands `admin` for.
    //
    // Classified by the parser rather than by sniffing the first word, so there is one
    // definition of what a schema change is and the check cannot disagree with the executor.
    match ctx.cluster.classify(text) {
        Ok(big_api::Sql::Ddl(_)) => {
            if let Some(refusal) = super::require(ctx.auth, req, Role::Admin) {
                return refusal;
            }
        }
        // A statement that does not translate is refused below, by the path that has an error
        // to report. Nothing here needs to decide that twice.
        Ok(big_api::Sql::Query(_)) | Err(_) => {}
    }

    let opts =
        QueryOptions { limits: None, timeout: ctx.query_timeout, cancel: ctx.cancel.clone() };
    match ctx.cluster.sql(text, &opts) {
        // The statement's `FORMAT` decides both the bytes and the type they are declared as: a
        // client that asked for TSV and was told `application/json` was answered twice, once
        // wrongly.
        Ok((value, answer)) => {
            Response::text(answer.format.content_type(), json::result_set(&answer, &value))
        }
        Err(e) => from_cluster(&e),
    }
}

/// `GET /table/{t}/records?after=<id>&limit=<n>` - every record in a table, in order.
///
/// Deliberately not `POST /query` with `All()`. That path builds the whole table's exists row
/// in memory before anything is returned, which is precisely the shape a listing must not have;
/// this one reads a shard at a time and stops at the first that fills the page. The two answer
/// the same question and cost differently, so both exist and the cheap one has its own name.
pub(super) fn records<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
    table: &str,
) -> Response {
    let page = match page(req) {
        Ok(p) => p,
        Err(why) => return Response::failure(422, "bad_parameter", &why),
    };
    // A listing with no ceiling would materialise the table one page at a time and then hand
    // over all of it anyway, so this route has a default where `/query` cannot: nobody is
    // relying on an unpaged answer from an endpoint that did not exist yesterday.
    let limit = page.limit.unwrap_or(DEFAULT_PAGE);
    match ctx.cluster.records(table, page.after, limit) {
        Ok(ids) => Response::ok(json::records(&ids, limit)),
        Err(e) => from_cluster(&e),
    }
}

/// One fact per line: `field record value`.
///
/// How the value is read is the field's kind to decide - a number for an integer, `true` or
/// `false` for a boolean, `key@seconds` for a time quantum field, a string for the rest.
///
/// A line format rather than JSON because ingest is the one route where volume matters, and a
/// hand-written JSON parser for the hot path would be the wrong thing to hand-write.
pub(super) fn import<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request, table: &str) -> Response {
    let body = match req.text() {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let mut parsed = Vec::new();
    for (n, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let (Some(field), Some(record), Some(value)) = (parts.next(), parts.next(), parts.next())
        else {
            return Response::failure(
                400,
                "malformed_line",
                &format!("line {}: expected `field record value`", n + 1),
            );
        };
        let Ok(record) = record.parse::<u64>() else {
            return Response::failure(
                400,
                "malformed_line",
                &format!("line {}: record must be a number", n + 1),
            );
        };
        parsed.push((field.to_string(), record, value.to_string()));
    }

    // The schema decides how a value is read, so the whole batch is resolved before any of it
    // is written - a batch that turns out to be malformed must not land halfway. In a cluster
    // it is resolved against this node's schema, which is every node's: a schema change is
    // applied everywhere or reported as half applied.
    let schema = ctx.cluster.schema();
    let Some(table_info) = schema.iter().find(|t| t.name == table) else {
        return Response::failure(404, "unknown_table", &format!("no table named `{table}`"));
    };

    let mut facts = Vec::with_capacity(parsed.len());
    for (field, record, value) in &parsed {
        let Some(info) = table_info.fields.iter().find(|f| f.name == *field) else {
            // In the body, not the URI: see `status::db` for why that makes it a 422.
            return Response::failure(
                422,
                "unknown_field",
                &format!("table `{table}` has no field named `{field}`"),
            );
        };
        let value = match info.kind {
            FieldKind::SignedInt => match value.parse::<i64>() {
                Ok(v) => FactValue::Signed(v),
                Err(_) => {
                    return Response::failure(
                        400,
                        "malformed_line",
                        &format!("`{field}` needs a signed number, got `{value}`"),
                    )
                }
            },
            FieldKind::Int | FieldKind::Decimal => match value.parse::<u64>() {
                Ok(v) => FactValue::Int(v),
                Err(_) => {
                    return Response::failure(
                        400,
                        "malformed_line",
                        &format!("`{field}` needs a number, got `{value}`"),
                    )
                }
            },
            FieldKind::Bool => match value.as_str() {
                "true" => FactValue::Bool(true),
                "false" => FactValue::Bool(false),
                other => {
                    return Response::failure(
                        400,
                        "malformed_line",
                        &format!("`{field}` needs true or false, got `{other}`"),
                    )
                }
            },
            // **A time quantum field takes `key@seconds`.** The field's kind decides how a
            // value is read, which is already true of every other kind here - and without a
            // moment the field can be filled and still have no views by day for a window to
            // read, which is what it had before this line existed. A key with no `@` is still
            // a key: the views are an addition, not a requirement.
            FieldKind::TimeQuantum => match value.rsplit_once('@') {
                None => FactValue::Key(value.clone()),
                Some((key, at)) => match at.parse::<i64>() {
                    Ok(unix_seconds) => FactValue::Time { value: key.to_string(), unix_seconds },
                    Err(_) => {
                        return Response::failure(
                            400,
                            "malformed_line",
                            &format!("`{field}` takes `key@seconds`, and `{at}` is not a number"),
                        )
                    }
                },
            },
            _ => FactValue::Key(value.clone()),
        };
        facts.push(OwnedFact { field: field.clone(), record: *record, value });
    }

    match ctx.cluster.import(table, &facts) {
        Ok(outcome) => Response::ok(json::wrote("imported", &outcome)),
        Err(e) => from_cluster(&e),
    }
}

/// One record id per line. Same shape as `/import`, so the same client code writes both.
pub(super) fn delete<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request, table: &str) -> Response {
    let body = match req.text() {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let mut records = Vec::new();
    for (n, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // The whole batch is parsed before any of it is deleted: a malformed line must not
        // leave half a batch removed, and unlike an import there is no undo for the half.
        let Ok(record) = line.parse::<u64>() else {
            return Response::failure(
                400,
                "malformed_line",
                &format!("line {}: record must be a number", n + 1),
            );
        };
        records.push(record);
    }

    match ctx.cluster.delete(table, &records) {
        Ok(outcome) => Response::ok(json::wrote("deleted", &outcome)),
        Err(e) => from_cluster(&e),
    }
}
