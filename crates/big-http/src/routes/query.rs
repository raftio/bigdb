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
use std::collections::HashMap;

pub(super) fn query<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request, table: &str) -> Response {
    if let Err((status, code, why)) = caught_up(ctx, req) {
        return Response::failure(status, code, &why);
    }
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
    let opts = QueryOptions {
        limits: None,
        timeout: ctx.query_timeout,
        cancel: ctx.cancel.clone(),
        database: req.param("database").map(|d| d.into_owned()),
        // A client's request names no shards. This node is the coordinator, and it scopes each
        // leg of its own fan-out - a client that could ask for a shard range would be a client
        // able to see half a cluster and call it an answer.
        shards: None,
    };
    match ctx.cluster.query(&scoped(req, table), text, &opts) {
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

/// `POST /sql` - one statement, answered as a result set.
///
/// No query string: `LIMIT` is part of the statement, and a second way to say the same thing is
/// a second way for a client to contradict itself. The deadline, the cancellation flag and the
/// memory ceiling are the ones `/query` gets, because a statement that runs long is the same
/// problem whichever language it was written in.
pub(super) fn sql<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
    principal: &crate::auth::Principal,
) -> Response {
    if let Err((status, code, why)) = caught_up(ctx, req) {
        return Response::failure(status, code, &why);
    }
    let text = match req.text() {
        Ok(t) => t.trim(),
        Err(e) => return e.into_response(),
    };
    // `?database=sales` says which database an unqualified name in the statement means. A
    // property of the request rather than of the text, because this route answers one statement
    // and remembers nothing - there is no session for a `USE` to leave one in. `bigctl` holds a
    // typed `USE` on the caller's behalf and sends it here.
    //
    // Built before the statement is classified because classifying needs it: a name resolves
    // against this database or against no database at all, and the two do not always agree.
    let opts = QueryOptions {
        limits: None,
        timeout: ctx.query_timeout,
        cancel: ctx.cancel.clone(),
        database: req.param("database").map(|d| d.into_owned()),
        // A client's request names no shards. This node is the coordinator, and it scopes each
        // leg of its own fan-out - a client that could ask for a shard range would be a client
        // able to see half a cluster and call it an answer.
        shards: None,
    };

    // **Translated once, for both decisions.** What the statement is decides what it costs and
    // what it does, and those have to be the same statement: classifying the text here and then
    // handing the *text* on to be translated again is two answers with nothing holding them
    // together. So the value authorised below is the value run below it.
    //
    // A statement that does not translate is reported here rather than deferred. It never runs -
    // there is nothing left to run it - so the report is the whole answer, and it is the same
    // 400 the second translation used to produce.
    let sql = match ctx.cluster.classify(text, &opts) {
        Ok(sql) => sql,
        Err(e) => return from_cluster(&e),
    };

    // **The route's guard is a floor, and the statement's own demands raise it.** The check in
    // `dispatch` runs before any body is decoded, which is what keeps it cheap and is why it
    // cannot know what statement arrived.
    //
    // Which objects a statement needs, and which privilege on each, is `Sql::demands`'s to say -
    // the rule belongs next to the variants it is about, where a kind of statement added later
    // cannot be added without answering for it. `Cluster::run` asks it, against the same
    // resolver this route's guard used, before any of the work starts.
    //
    // Nothing is re-verified here. The principal was resolved once by `refuse`, and re-running
    // the credential check would mean a second argon2 verification on every statement.
    // **Two statements are answered here rather than by the cluster**, and it is the one place
    // this route decides anything about what a statement means. The reason is where the state
    // lives: the cancellation flag is minted per connection in this crate, so the map from an id
    // to a flag is this crate's too - and reaching down into it from `Cluster::run` would put a
    // process-local registry behind an interface that is about a distributed database.
    match &sql {
        big_embed::Sql::Kill(id) => {
            if let Some(denied) = refuse_unauthorised(ctx, &principal.who(), &sql) {
                return denied;
            }
            let killed = u64::from(ctx.running.kill(id));
            let set = big_embed::one_cell("killed", big_embed::Datum::Int(killed.into()));
            return stamp(
                ctx,
                Response::text(
                    big_embed::Format::default().content_type(),
                    json::result_set(big_embed::Format::default(), &set),
                ),
            );
        }
        big_embed::Sql::Show(show) if matches!(show.what, big_embed::SqlShown::Processlist) => {
            if let Some(denied) = refuse_unauthorised(ctx, &principal.who(), &sql) {
                return denied;
            }
            let set = processlist(ctx);
            return stamp(
                ctx,
                Response::text(show.format.content_type(), json::result_set(show.format, &set)),
            );
        }
        _ => {}
    }

    // **Registered here, and only for the statements that can run long enough to be worth
    // stopping.** The flag is already in `opts`; what this adds is the address. The guard forgets
    // the entry on every path out of this function including a panic, which is what keeps a
    // finished query from staying in the listing for ever and being "killed" without effect.
    let _running = ctx.cancel.as_ref().map(|cancel| {
        ctx.running.register(
            ctx.cluster.node_name(),
            std::sync::Arc::clone(cancel),
            match principal.who() {
                big_rbac::Who::Role(r) => Some(r.clone()),
                big_rbac::Who::Trusted => None,
            },
            opts.database().to_string(),
            text.to_string(),
        )
    });

    match ctx.cluster.run(sql, &principal.who(), &opts) {
        // The statement's `FORMAT` decides both the bytes and the type they are declared as: a
        // client that asked for TSV and was told `application/json` was answered twice, once
        // wrongly.
        Ok((set, format)) => {
            stamp(ctx, Response::text(format.content_type(), json::result_set(format, &set)))
        }
        Err(e) => from_cluster(&e),
    }
}

/// `X-Big-Txn: <node>/<transaction>` — where this node's history had got to when it answered.
///
/// **Read after the write rather than carried out of it**, and that is a deliberate
/// over-approximation. A commit that landed between this write and this read makes the number
/// larger than the one that actually carried the facts, which can only make a later
/// `?min_txn=` wait for something *further on* — never for something earlier. Threading a
/// transaction id back out through `WriteOutcome` and the peer wire would buy exactness that
/// read-your-writes has no use for.
///
/// **The node name is half the value.** A transaction id is monotonic within one file and means
/// nothing outside it, so a bare number would be a number a client could send to the wrong node
/// and be quietly misled by.
fn stamp<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, r: Response) -> Response {
    let node = &ctx.cluster.config().this().name;
    r.with_header("X-Big-Txn", format!("{node}/{}", ctx.api().txn_id()))
}

/// How long a `?min_txn=` may wait to be caught up, before it is a timeout.
const DEFAULT_WAIT_MS: u64 = 1_000;

/// The ceiling on that, whatever the request asks for. A read that waits longer than this is a
/// read that should have been sent to the node that did the write.
const MAX_WAIT_MS: u64 = 60_000;

/// `?min_txn=<node>/<transaction>` — do not answer until this node is at least there.
///
/// Send back exactly what `X-Big-Txn` gave you. What it buys is read-your-writes without a
/// sleep: a client that wrote and wants to see its own write asks to be caught up rather than
/// guessing how long that takes.
///
/// **Refused when the node does not match**, rather than waited out and reported as a timeout.
/// A transaction id from another node names a history this one does not have, so waiting for it
/// would be waiting for something that cannot arrive - a `409` says that in one round trip.
/// Through `bigproxy` the client does not choose which node answers, so this parameter is for a
/// client talking to a node directly.
fn caught_up<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
) -> core::result::Result<(), (u16, &'static str, String)> {
    let Some(asked) = req.param("min_txn") else { return Ok(()) };
    let Some((node, txn)) = asked.split_once('/') else {
        return Err((
            422,
            "bad_parameter",
            "min_txn is the value of an X-Big-Txn header, `<node>/<transaction>`".to_string(),
        ));
    };
    let Ok(txn) = txn.parse::<big_pager::TxnId>() else {
        return Err((422, "bad_parameter", format!("min_txn names no transaction: `{txn}`")));
    };
    let here = &ctx.cluster.config().this().name;
    if node != here {
        return Err((
            409,
            "wrong_node",
            format!(
                "min_txn is node `{node}`'s transaction and this is node `{here}`; a transaction \
                 id means nothing on another node"
            ),
        ));
    }

    let wait = match req.param("wait") {
        None => DEFAULT_WAIT_MS,
        Some(ms) => match ms.parse::<u64>() {
            Ok(ms) => ms.min(MAX_WAIT_MS),
            Err(_) => {
                return Err((
                    422,
                    "bad_parameter",
                    format!("wait is a number of milliseconds, got `{ms}`"),
                ))
            }
        },
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait);
    if ctx.api().wait_for_txn(txn, deadline) {
        return Ok(());
    }
    Err((
        504,
        "not_caught_up",
        format!("this node is at transaction {} and was asked for {txn}", ctx.api().txn_id()),
    ))
}

/// The table a request names, with `?database=` folded in.
///
/// **One meaning for the parameter, on every route that takes a table.** The route table says
/// `?database=<name>` scopes an unqualified table name, and `refuse` reads it on every route to
/// build the RBAC object - but only `/sql` and `/query` ever threaded it into the work. So
/// `/import`, `/delete` and `/records` accepted the parameter, authorised against it, and then
/// resolved the table in the default database anyway. `/import` was worse still: it also
/// compared the raw path segment against `TableInfo::name`, which is the *bare* name, so a
/// qualified `sales.orders` matched nothing either and the table was unreachable by any
/// spelling.
///
/// A qualified path wins over a disagreeing parameter, being the more specific of the two.
/// Everything downstream takes a qualified name already - `TableRef::parse` is the one decoder
/// - so this returns a `String` rather than trying to pass a database alongside it.
fn scoped(req: &Request, table: &str) -> String {
    match req.param("database") {
        // `check_name` refuses a `.` in a name, so a segment holding one is already qualified.
        Some(database) if !table.contains('.') => format!("{database}.{table}"),
        _ => table.to_string(),
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
    if let Err((status, code, why)) = caught_up(ctx, req) {
        return Response::failure(status, code, &why);
    }
    let page = match page(req) {
        Ok(p) => p,
        Err(why) => return Response::failure(422, "bad_parameter", &why),
    };
    // A listing with no ceiling would materialise the table one page at a time and then hand
    // over all of it anyway, so this route has a default where `/query` cannot: nobody is
    // relying on an unpaged answer from an endpoint that did not exist yesterday.
    let limit = page.limit.unwrap_or(DEFAULT_PAGE);
    match ctx.cluster.records(&scoped(req, table), page.after, limit) {
        Ok(ids) => Response::ok(json::records(&ids, limit)),
        Err(e) => from_cluster(&e),
    }
}

/// `?ack=commit|queued` - whether the caller waits for the commit that carries its facts.
///
/// **Refused by name where it is not offered, never quietly downgraded.** A client that asked
/// to be answered early and was made to wait instead sees latency it cannot explain and has no
/// way to find out why; a `422` naming the flag is something an operator can act on.
///
/// In a cluster it is refused outright. `WriteOutcome::missed` is only knowable once every copy
/// has answered, so a node acking before it has written would turn the coordinator's report
/// from a statement about reachability into a claim about durability that is not true.
/// The code and the sentence, for the caller to shape - the way `page` hands back a reason
/// rather than a whole `Response`.
type Refusal = (&'static str, String);

fn ack_of<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
) -> core::result::Result<Ack, Refusal> {
    match req.param("ack").as_deref() {
        None | Some("commit") => Ok(Ack::Sync),
        Some("queued") => {
            if !ctx.cluster.writes_alone() {
                return Err((
                    "ack_not_available",
                    "this node has peers; a write is answered when every copy has taken it"
                        .to_string(),
                ));
            }
            if !ctx.api().takes_async_writes() {
                return Err((
                    "ack_not_available",
                    "this server was not started with --write-async".to_string(),
                ));
            }
            Ok(Ack::Async)
        }
        Some(other) => Err(("bad_parameter", format!("ack takes commit or queued, got `{other}`"))),
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

    // The schema decides how a value is read, so the whole batch is resolved before any of it
    // is written - a batch that turns out to be malformed must not land halfway. In a cluster
    // it is resolved against this node's schema, which is every node's: a schema change is
    // applied everywhere or reported as half applied.
    let ack = match ack_of(ctx, req) {
        Ok(ack) => ack,
        Err((code, why)) => return Response::failure(422, code, &why),
    };

    let name = scoped(req, table);
    let target = big_db::TableRef::parse(&name);
    let schema = ctx.cluster.schema();
    // Both halves, because `TableInfo::name` is the bare name and two databases may each hold
    // a table called `orders`. The same comparison `Cluster::sql_insert` makes.
    let Some(table_info) =
        schema.iter().find(|t| t.name == target.table && t.database == target.database)
    else {
        return Response::failure(404, "unknown_table", &format!("no table named `{name}`"));
    };

    // Field lookup, built once. A linear scan per line is fine for the four-field table in a
    // test and is quadratic in the shape this route exists for: a wide table and a body full
    // of lines. A map costs one hash per line instead.
    let by_name: HashMap<&str, &FieldInfo> =
        table_info.fields.iter().map(|f| (f.name.as_str(), f)).collect();

    let facts = match parse_body(body, &by_name) {
        Ok(f) => f,
        Err(e) => return e.into_response(table),
    };

    // **Borrowed all the way in, when this node writes alone.** The facts point into the request
    // body, which outlives the write. Copying them into `OwnedFact` first - a `String` for the
    // field and another for a keyed value, per fact - was two allocations each on the one route
    // whose purpose is volume, and the very next line inside the cluster borrowed them straight
    // back. A node with peers still pays, because a batch that has to be shipped needs to own
    // what it ships.
    let outcome = if ctx.cluster.writes_alone() {
        // Answering early is a single-node path, so it forks here rather than inside the
        // cluster: `import_borrowed` is the one that already knows it is writing alone.
        if ack == Ack::Async {
            // No `X-Big-Txn` on an early answer, and that is the honest shape: there is no
            // transaction yet to name. A client that needs to read its own write back has to
            // ask for `?ack=commit`, which is the trade it made.
            return match ctx.api().import_acked(&name, &facts, ack) {
                Ok(a) => Response::ok(json::queued("imported", a.count)),
                Err(e) => crate::status::response_for(&e),
            };
        }
        ctx.cluster.import_borrowed(&name, &facts)
    } else {
        let owned: Vec<OwnedFact> = facts.iter().map(OwnedFact::from_fact).collect();
        ctx.cluster.import(&name, &owned)
    };
    match outcome {
        Ok(outcome) => stamp(ctx, Response::ok(json::wrote("imported", &outcome))),
        Err(e) => from_cluster(&e),
    }
}

/// What a line can be wrong about, and which line it was.
#[derive(Debug)]
struct ParseError {
    line: usize,
    status: u16,
    code: &'static str,
    what: String,
}

impl ParseError {
    fn into_response(self, table: &str) -> Response {
        let ParseError { line, status, code, what } = self;
        // `unknown_field` names the table rather than the line, because that is the fact the
        // caller has to act on; everything else is about one line and says which.
        if code == "unknown_field" {
            return Response::failure(
                status,
                code,
                &format!("table `{table}` has no field named `{what}`"),
            );
        }
        Response::failure(status, code, &format!("line {line}: {what}"))
    }
}

/// Below this, one thread parses the lot.
///
/// A body is at most eight megabytes and a small one is over in microseconds, so fanning out
/// unconditionally would charge every little write the price of spawning threads. Chosen from
/// the profile rather than taste: parsing is a quarter of an import's CPU at the sizes `bigctl`
/// sends, and nothing at the sizes a hand-written `curl` does.
const PARALLEL_PARSE_MIN: usize = 256 * 1024;

/// Every fact in the body, borrowed from it.
///
/// Split on line boundaries and parsed on several threads when the body is large enough to pay
/// for them. The pieces are independent - a fact is one line, and a line means the same thing
/// wherever it sits - so the only thing the split has to get right is not cutting a line in
/// half, and the only thing the join has to get right is which line a refusal names.
fn parse_body<'a>(
    body: &'a str,
    by_name: &HashMap<&'a str, &'a FieldInfo>,
) -> Result<Vec<Fact<'a>>, ParseError> {
    let threads = core::cmp::min(
        std::thread::available_parallelism().map_or(1, |n| n.get()),
        body.len() / PARALLEL_PARSE_MIN,
    );
    if threads < 2 {
        let mut facts = Vec::new();
        parse_into(body, by_name, 0, &mut facts)?;
        return Ok(facts);
    }

    let pieces = split_on_lines(body, threads);
    let mut results: Vec<Result<(usize, Vec<Fact<'a>>), ParseError>> =
        Vec::with_capacity(pieces.len());
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(pieces.len());
        for piece in &pieces {
            handles.push(scope.spawn(move || {
                let mut facts = Vec::new();
                // Line numbers are fixed up below: a piece cannot know how many lines came
                // before it without counting them, and counting them here would be the serial
                // pass this is trying to avoid.
                let lines = parse_into(piece, by_name, 0, &mut facts)?;
                Ok((lines, facts))
            }));
        }
        for h in handles {
            results.push(h.join().expect("parsing a line must not panic"));
        }
    });

    let mut facts = Vec::new();
    let mut base = 0usize;
    for result in results {
        match result {
            Ok((lines, mut part)) => {
                facts.append(&mut part);
                base += lines;
            }
            // The first refusal wins, and it is the first in *body* order because the results
            // are walked in order - so two malformed lines in different pieces name the earlier
            // one, exactly as the single-threaded parse did.
            Err(mut e) => {
                e.line += base;
                return Err(e);
            }
        }
    }
    Ok(facts)
}

/// Cuts `body` into at most `n` pieces, never through a line.
fn split_on_lines(body: &str, n: usize) -> Vec<&str> {
    let mut pieces = Vec::with_capacity(n);
    let bytes = body.as_bytes();
    let target = body.len().div_ceil(n);
    let mut start = 0;
    while start < body.len() {
        let mut end = core::cmp::min(start + target, body.len());
        // Forward to the end of whatever line the cut landed in the middle of.
        while end < body.len() && bytes[end] != b'\n' {
            end += 1;
        }
        if end < body.len() {
            end += 1;
        }
        pieces.push(&body[start..end]);
        start = end;
    }
    pieces
}

/// Parses one piece, appending to `facts`, and reports how many lines it held.
fn parse_into<'a>(
    piece: &'a str,
    by_name: &HashMap<&'a str, &'a FieldInfo>,
    base: usize,
    facts: &mut Vec<Fact<'a>>,
) -> Result<usize, ParseError> {
    let bytes = piece.as_bytes();
    let mut seen = 0usize;
    let mut start = 0usize;
    // **Cut by hand rather than with `str::lines`.** `lines` is `split_inclusive('\n')`, which
    // goes through the generic pattern machinery: a `CharSearcher` set up and torn down per
    // line, and a call graph put that at 5.5% of the daemon during an import. The lines here
    // are about twenty-six bytes, so the setup is most of the work.
    //
    // Same lines `str::lines` yields, including the count - which matters, because `seen` is
    // what a refusal names. A trailing newline does not produce a final empty line; an empty
    // piece produces none.
    while start < bytes.len() {
        let end = match newline(&bytes[start..]) {
            Some(i) => start + i,
            None => bytes.len(),
        };
        // Both ends are `\n` or a piece boundary, so slicing here is always on a character
        // boundary and the bounds check is all it costs.
        let line = &piece[start..end];
        start = end + 1;
        seen += 1;
        let n = base + seen;
        // ASCII, deliberately. This format's framing - the separator, the line ending, the
        // padding an editor leaves - is all ASCII, and `str::trim` decodes a `char` at each end
        // to say so. A non-breaking space in a value is then data rather than framing, which is
        // the reading a keyed value wants: two keys that differ by one are two keys.
        let line = line.trim_ascii();
        if line.is_empty() {
            continue;
        }
        let Some((field, record, value)) = three(line) else {
            return Err(ParseError {
                line: n,
                status: 400,
                code: "malformed_line",
                what: "expected `field record value`".to_string(),
            });
        };
        let Ok(record) = record.parse::<u64>() else {
            return Err(ParseError {
                line: n,
                status: 400,
                code: "malformed_line",
                what: "record must be a number".to_string(),
            });
        };
        let Some((name, info)) = by_name.get_key_value(field) else {
            // In the body, not the URI: see `status::db` for why that makes it a 422.
            return Err(ParseError {
                line: n,
                status: 422,
                code: "unknown_field",
                what: field.to_string(),
            });
        };
        // **The schema's copy of the name, not this line's.** Identical strings, and that is the
        // point: every fact naming `amount` now carries the same slice, so `big_embed::apply`
        // matches a fact to its resolved field by address instead of by `memcmp`. It also
        // outlives the line, which a borrowed fact wants anyway.
        let field = *name;
        // **How a value is read is `big_embed::fact`'s to decide, not this route's.** A SQL
        // `INSERT` writes the same facts into the same fields, and two implementations of "what
        // does `true` mean on a boolean field" would be two conventions in one table.
        facts.push(match big_embed::fact::from_text(field, info, record, value) {
            Ok(fact) => fact,
            Err(e) => {
                return Err(ParseError {
                    line: n,
                    status: 400,
                    code: "malformed_line",
                    what: e.why(field, value),
                })
            }
        });
    }
    Ok(seen)
}

/// Index of the first newline in `bytes`, eight bytes at a time.
///
/// **Why this is not `iter().position`.** A byte at a time is a compare and a branch per byte,
/// and the loop this feeds runs over every byte of an eight-megabyte body. The word trick below
/// answers eight bytes with three arithmetic operations, which is the whole of the saving.
///
/// `x - 0x0101..` borrows out of a zero byte and lights its high bit; `& !x` drops the bytes
/// that were already high, and `& 0x8080..` keeps one bit per byte. Borrows travel towards the
/// more significant end, so a byte can only be lit falsely *above* a real zero - never below
/// it - and the lowest lit byte is therefore the first match. Read little-endian whatever the
/// machine is, so "lowest" means "first" here too.
fn newline(bytes: &[u8]) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    const WANTED: u64 = ONES * b'\n' as u64;

    let mut i = 0;
    while i + 8 <= bytes.len() {
        let word = u64::from_le_bytes(bytes[i..i + 8].try_into().expect("eight bytes"));
        let x = word ^ WANTED;
        let hit = x.wrapping_sub(ONES) & !x & HIGH;
        if hit != 0 {
            return Some(i + (hit.trailing_zeros() >> 3) as usize);
        }
        i += 8;
    }
    bytes[i..].iter().position(|b| *b == b'\n').map(|k| i + k)
}

/// `field record value`, split on the first two spaces.
///
/// Byte scanning rather than `splitn(3, ' ')`. Splitting on a `char` goes through the generic
/// pattern machinery, and at twenty million lines that machinery was 6.7% of the daemon's CPU -
/// for finding two spaces. The value keeps whatever spaces it contains, which is what `splitn`
/// did and what a keyed value needs.
fn three(line: &str) -> Option<(&str, &str, &str)> {
    let bytes = line.as_bytes();
    let first = bytes.iter().position(|b| *b == b' ')?;
    let second = first + 1 + bytes[first + 1..].iter().position(|b| *b == b' ')?;
    Some((&line[..first], &line[first + 1..second], &line[second + 1..]))
}

/// One record id per line. Same shape as `/import`, so the same client code writes both.
pub(super) fn delete<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request, table: &str) -> Response {
    let ack = match ack_of(ctx, req) {
        Ok(ack) => ack,
        Err((code, why)) => return Response::failure(422, code, &why),
    };
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

    let name = scoped(req, table);
    if ack == Ack::Async {
        return match ctx.api().delete_acked(&name, &records, ack) {
            Ok(a) => Response::ok(json::queued("deleted", a.count)),
            Err(e) => crate::status::response_for(&e),
        };
    }
    match ctx.cluster.delete(&name, &records) {
        Ok(outcome) => stamp(ctx, Response::ok(json::wrote("deleted", &outcome))),
        Err(e) => from_cluster(&e),
    }
}

/// `SHOW PROCESSLIST`: the queries running on this node, oldest first.
///
/// **Node-local, and the `node` column says so.** A coordinator holds the ranges it owns; a
/// cluster-wide listing would be a fan-out, and one that showed peers' legs of a fanned-out query
/// as separate rows would be more confusing than the honest local answer.
fn processlist<P: PagerMut + Sync>(ctx: &Ctx<'_, P>) -> big_embed::ResultSet {
    let columns =
        ["id", "elapsed_ms", "role", "database", "statement"].map(str::to_string).to_vec();
    let rows = ctx
        .running
        .running()
        .into_iter()
        .map(|r| {
            vec![
                big_embed::Datum::Text(r.id.clone()),
                big_embed::Datum::Int(i128::from(
                    u64::try_from(r.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                )),
                r.role.clone().map_or(big_embed::Datum::Null, big_embed::Datum::Text),
                big_embed::Datum::Text(r.database.clone()),
                big_embed::Datum::Text(r.text.clone()),
            ]
        })
        .collect();
    big_embed::ResultSet { columns, rows }
}

/// The denial a statement earns, if it earns one.
///
/// **The same check `Cluster::run` makes, and reported as the same error.** The two statements
/// this route answers itself would otherwise be the two with their own authorisation path, which
/// is how a fence comes to be enforced in one place and written in two. Asking `Sql::demands` and
/// building the identical `Denied` keeps the rule where the variants are.
fn refuse_unauthorised<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    who: &big_rbac::Who,
    sql: &big_embed::Sql,
) -> Option<Response> {
    for demand in sql.demands() {
        if !ctx.cluster.local().allows(who, demand) {
            let denied = big_embed::ApiError::Denied(Box::new(big_rbac::Denied {
                role: match who {
                    big_rbac::Who::Role(r) => Some(r.clone()),
                    big_rbac::Who::Trusted => None,
                },
                privilege: demand.privilege,
                on: demand.on.to_owned(),
            }));
            return Some(from_cluster(&big_cluster::ClusterError::Local(denied)));
        }
    }
    None
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    fn field(name: &str, kind: FieldKind) -> FieldInfo {
        FieldInfo {
            name: name.to_string(),
            kind,
            bit_depth: 20,
            scale: 0,
            granularity: Vec::new(),
            members: Vec::new(),
        }
    }

    /// The word-at-a-time scan against the obvious one, at every length and every offset a
    /// newline can sit at - including the tail the word loop cannot reach and the boundary
    /// between the two.
    #[test]
    fn the_newline_scan_finds_what_a_byte_at_a_time_scan_finds() {
        for len in 0..40usize {
            let clean = vec![b'x'; len];
            assert_eq!(newline(&clean), None, "len {len}: no newline to find");
            for at in 0..len {
                let mut bytes = clean.clone();
                bytes[at] = b'\n';
                assert_eq!(
                    newline(&bytes),
                    bytes.iter().position(|b| *b == b'\n'),
                    "len {len}, newline at {at}"
                );
                // A second one further along must not win over the first.
                for also in at + 1..len {
                    let mut two = bytes.clone();
                    two[also] = b'\n';
                    assert_eq!(newline(&two), Some(at), "len {len}, {at} then {also}");
                }
            }
        }
    }

    /// Bytes that are *nearly* a newline, because the trick works on `x ^ 0x0a0a..` and a byte
    /// one away from the pattern is where a borrow could light the wrong lane.
    #[test]
    fn the_newline_scan_is_not_fooled_by_neighbouring_bytes() {
        for fill in [0u8, 1, 9, 11, 0x0a ^ 0x80, 0x7f, 0x80, 0xff] {
            for len in 1..24usize {
                for at in 0..len {
                    let mut bytes = vec![fill; len];
                    bytes[at] = b'\n';
                    let want = bytes.iter().position(|b| *b == b'\n');
                    assert_eq!(newline(&bytes), want, "fill {fill:#04x}, len {len}, at {at}");
                }
                let bytes = vec![fill; len];
                assert_eq!(newline(&bytes), None, "fill {fill:#04x}, len {len}: none");
            }
        }
    }

    /// The hand-cut loop has to yield exactly what `str::lines` yielded, count included: `seen`
    /// is what a refusal names, so an off-by-one here is a message pointing at the wrong line.
    #[test]
    fn cutting_lines_by_hand_matches_str_lines() {
        let amount = field("amount", FieldKind::Int);
        let by_name: HashMap<&str, &FieldInfo> = [("amount", &amount)].into_iter().collect();
        for body in [
            "",
            "\n",
            "\n\n",
            "amount 1 1",
            "amount 1 1\n",
            "amount 1 1\n\n",
            "amount 1 1\namount 2 2",
            "amount 1 1\namount 2 2\n",
            "\namount 1 1\n\namount 2 2\n\n",
            "  amount 1 1  \r\n\t\namount 2 2\r\n",
        ] {
            let mut facts = Vec::new();
            let seen = parse_into(body, &by_name, 0, &mut facts).expect("all of these parse");
            assert_eq!(seen, body.lines().count(), "line count for {body:?}");
        }
    }

    /// Every fact of one field carries the schema's own slice, which is what lets `big_embed`
    /// match a fact to its field by address. Same string either way - this is about *which*
    /// copy of it.
    #[test]
    fn a_fact_carries_the_schema_s_copy_of_the_field_name() {
        let amount = field("amount", FieldKind::Int);
        let by_name: HashMap<&str, &FieldInfo> =
            [(amount.name.as_str(), &amount)].into_iter().collect();
        let body = "amount 1 1\namount 2 2\n";
        let mut facts = Vec::new();
        parse_into(body, &by_name, 0, &mut facts).expect("both lines parse");
        assert_eq!(facts.len(), 2);
        for fact in &facts {
            assert_eq!(fact.field(), "amount");
            assert!(
                core::ptr::eq(fact.field().as_ptr(), amount.name.as_ptr()),
                "the fact should point at the schema's name, not at the body"
            );
        }
    }

    #[test]
    fn a_line_splits_on_its_first_two_spaces() {
        assert_eq!(three("amount 7 42"), Some(("amount", "7", "42")));
        // The value keeps its own spaces, which is what a keyed value needs and what
        // `splitn(3, ' ')` did.
        assert_eq!(three("city 7 New York"), Some(("city", "7", "New York")));
        assert_eq!(three("amount 7"), None);
        assert_eq!(three("amount"), None);
    }

    #[test]
    fn splitting_never_cuts_a_line_in_half() {
        let body: String = (0..5000).map(|i| format!("amount {i} {i}\n")).collect();
        for n in [1usize, 2, 3, 7, 64] {
            let pieces = split_on_lines(&body, n);
            assert_eq!(pieces.concat(), body, "n={n}: pieces must rebuild the body");
            for piece in &pieces {
                assert!(
                    piece.is_empty()
                        || piece.ends_with('\n')
                        || piece.as_ptr() as usize + piece.len()
                            == body.as_ptr() as usize + body.len(),
                    "n={n}: a piece ended mid-line"
                );
            }
        }
    }

    #[test]
    fn a_refusal_names_the_right_line_across_threads() {
        // Big enough to take the parallel path, with the bad line far enough in to land in a
        // piece other than the first. Getting the fix-up wrong is invisible until an operator
        // goes looking at the line a refusal named and finds it fine.
        let mut body = String::new();
        let bad_at = 40_000usize;
        for i in 1..=80_000usize {
            if i == bad_at {
                body.push_str("amount notanumber 5\n");
            } else {
                body.push_str(&format!("amount {i} {i}\n"));
            }
        }
        assert!(body.len() > 2 * PARALLEL_PARSE_MIN, "body must trigger the parallel path");

        let amount = field("amount", FieldKind::Int);
        let by_name: HashMap<&str, &FieldInfo> = [("amount", &amount)].into_iter().collect();

        let err = parse_body(&body, &by_name).expect_err("the bad line must be refused");
        assert_eq!(err.line, bad_at);
        assert_eq!(err.code, "malformed_line");
    }

    #[test]
    fn the_parallel_and_serial_paths_agree() {
        let mut body = String::new();
        for i in 1..=60_000usize {
            body.push_str(&format!("amount {i} {}\n", i % 1024));
        }
        let amount = field("amount", FieldKind::Int);
        let by_name: HashMap<&str, &FieldInfo> = [("amount", &amount)].into_iter().collect();

        let parallel = parse_body(&body, &by_name).expect("valid");
        let mut serial = Vec::new();
        parse_into(&body, &by_name, 0, &mut serial).expect("valid");

        assert_eq!(parallel.len(), serial.len());
        // Order matters: `apply` replays in arrival order, so last-write-wins per record means
        // what it would have meant in one thread.
        for (a, b) in parallel.iter().zip(serial.iter()) {
            assert_eq!(format!("{a:?}"), format!("{b:?}"));
        }
    }
}
