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

//! `GET /watch` — one `SELECT`, re-answered when its answer changes.
//!
//! **A live query, not a change feed, and the difference is the whole design.** This engine
//! keeps no log of logical changes: copy-on-write preserves old *pages*, which says nothing
//! about which facts moved. Building a feed of changed rows would mean either diffing two
//! snapshots or writing a second log to disk, and both are large. But an answer here is a
//! *number* — a count, a sum, a top ten — so re-running the statement and pushing the answer
//! when it differs is not a compromise: it is the shape the data already has, and it is what a
//! dashboard wanted in the first place.
//!
//! **What wakes it.** On a node writing alone, the store's commit condvar — the one
//! `?min_txn=` waits on — so a push follows a commit immediately. On a node with peers it is
//! the clock, because a commit on another node notifies nothing here; the interval is then the
//! whole trigger and the documentation says so rather than implying a promptness that is not
//! there.
//!
//! **The grant is re-checked on every push.** A `REVOKE` while a stream is open has to cut it
//! off, and a check done once at subscribe is a credential that outlives its own revocation for
//! as long as the client cares to hold the socket.
//!
//! **Every subscriber holds a worker for the life of its connection.** That is the real cost of
//! this route on a blocking server, so it is capped, and the cap is off by default: with
//! `--watch-max 0` the route answers `503` and nothing can be held open at all.

use super::{Ctx, QueryOptions};
use crate::json;
use big_pager::PagerMut;
use big_wire::{Chunked, Request, Response, Streaming};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How often a stream looks up even when nothing has woken it.
///
/// A ceiling on staleness in a cluster, and on this node a bound on how long a stream takes to
/// notice the server is stopping.
const DEFAULT_INTERVAL: Duration = Duration::from_millis(1_000);

/// The shortest interval a client may ask for. Below this it is a busy loop with extra steps.
const MIN_INTERVAL: Duration = Duration::from_millis(50);

/// The longest. Past it, a subscription is a poll and the client should be polling.
const MAX_INTERVAL: Duration = Duration::from_secs(300);

/// Whether the request may open a stream, and what it asked for.
pub(super) fn subscribe<P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &Request,
    principal: &crate::auth::Principal,
) -> Result<(Response, super::Watch), Box<Response>> {
    if ctx.watch_max == 0 {
        return Err(Box::new(Response::failure(
            503,
            "watch_unavailable",
            "this server was not started with --watch-max",
        )));
    }
    let Some(sql) = req.param("sql") else {
        return Err(Box::new(Response::failure(
            422,
            "bad_parameter",
            "watch takes the statement as ?sql=<urlencoded SELECT>",
        )));
    };
    let sql = sql.into_owned();

    let interval = match req.param("interval") {
        None => DEFAULT_INTERVAL,
        Some(ms) => match ms.parse::<u64>() {
            Ok(ms) => Duration::from_millis(ms).clamp(MIN_INTERVAL, MAX_INTERVAL),
            Err(_) => {
                return Err(Box::new(Response::failure(
                    422,
                    "bad_parameter",
                    &format!("interval is a number of milliseconds, got `{ms}`"),
                )))
            }
        },
    };

    // **Refused here rather than at the first push.** A statement that is not a `SELECT`, or
    // names a table that is not there, is a mistake the client should learn about in the
    // response to its own request - not as a stream that opens successfully and then closes.
    let opts = options(ctx, req);
    match ctx.cluster.classify(&sql, &opts) {
        Ok(big_embed::Sql::Query(_)) => {}
        Ok(_) => {
            return Err(Box::new(Response::failure(
                422,
                "not_a_query",
                "watch re-runs a SELECT; it does not write, and a statement that writes would \
                 write once per push",
            )))
        }
        Err(e) => return Err(Box::new(super::from_cluster(&e))),
    }

    // Held for the life of the stream and given back in `run`, whatever ends it.
    let live = ctx.watching.fetch_add(1, Ordering::SeqCst);
    if live >= ctx.watch_max {
        ctx.watching.fetch_sub(1, Ordering::SeqCst);
        return Err(Box::new(
            Response::failure(
                503,
                "server_busy",
                "this server is already holding as many subscriptions as it allows",
            )
            .with_header("Retry-After", 1),
        ));
    }

    let head = Streaming::ok("text/event-stream")
        .with_header("X-Big-Node", &ctx.cluster.config().this().name);
    let watch = super::Watch {
        sql,
        database: req.param("database").map(|d| d.into_owned()),
        interval,
        who: principal.who(),
        head,
    };
    // The status the log should record. The bytes are the stream's, not this one's.
    Ok((Response::ok(String::new()), watch))
}

/// Pushes the answer, and pushes it again whenever it changes, until somebody leaves.
///
/// Returns when the client goes away, the server stops, or the caller loses the privilege it
/// subscribed with. A write that fails is the ordinary way this ends: the reader closed the
/// connection, which is a subscription being cancelled rather than an error worth logging.
pub(crate) fn run<P: PagerMut + Sync>(
    state: &crate::State<P>,
    watch: &super::Watch,
    out: &mut Chunked<'_>,
) {
    // Given back however this returns, including on a panic in the loop below: a counter that
    // only decrements on the happy path is a cap that ratchets shut.
    let _held = Subscription(&state.watching);

    let api = state.cluster.local();
    let alone = state.cluster.writes_alone();
    let mut last: Option<String> = None;
    // Where this node's history was when the last push was built, so a reconnecting client can
    // see whether it missed anything.
    let mut seen = api.txn_id();

    while !state.stopping.load(Ordering::Relaxed) {
        let opts = QueryOptions {
            limits: None,
            timeout: state.config.query_timeout,
            cancel: None,
            database: watch.database.clone(),
            shards: None,
        };

        // Re-asked every time round. A revocation has to end the stream, and a check made once
        // at subscribe would outlive it for as long as the client held the socket.
        let answer = match state.cluster.classify(&watch.sql, &opts) {
            Ok(sql) => state.cluster.run(sql, &watch.who, &opts),
            Err(e) => Err(e),
        };
        let (set, format) = match answer {
            Ok(pair) => pair,
            Err(e) => {
                // Said once, then the stream ends. A subscription that keeps pushing the same
                // failure is a client that never finds out it should stop reconnecting.
                let _ = event(
                    out,
                    "error",
                    &seen_at(state, seen),
                    &json::error(e.code(), &e.to_string()),
                );
                return;
            }
        };

        let body = json::result_set(format, &set);
        seen = api.txn_id();
        if last.as_deref() != Some(body.as_str()) {
            if event(out, "answer", &seen_at(state, seen), &body).is_err() {
                // The reader is gone. That is what unsubscribing looks like from here.
                return;
            }
            last = Some(body);
        }

        wait(state, api, alone, seen, watch.interval);
    }
}

/// Blocks until something might have changed, or the interval is up.
///
/// On a node writing alone the commit condvar is exact: nothing can change without a commit, so
/// a wake means there is something to look at and a timeout means there is not. With peers a
/// commit elsewhere notifies nothing here, so the interval is the whole trigger — which is why
/// the interval exists at all rather than being an implementation detail.
fn wait<P: PagerMut + Sync>(
    state: &crate::State<P>,
    api: &big_embed::Api<P>,
    alone: bool,
    seen: big_pager::TxnId,
    interval: Duration,
) {
    if !alone {
        // Chopped up so a stopping server is noticed promptly rather than after a long
        // interval, the way the steward slices its own sleep.
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline && !state.stopping.load(Ordering::Relaxed) {
            std::thread::sleep(interval.min(Duration::from_millis(50)));
        }
        return;
    }
    api.wait_for_txn(seen + 1, Instant::now() + interval);
}

/// `<node>/<transaction>`, the same identifier `X-Big-Txn` uses.
fn seen_at<P: PagerMut + Sync>(state: &crate::State<P>, txn: big_pager::TxnId) -> String {
    format!("{}/{txn}", state.cluster.config().this().name)
}

/// One server-sent event. `data` is written as a single line because everything here is JSON,
/// which has no newlines outside strings and escapes them inside.
fn event(out: &mut Chunked<'_>, name: &str, id: &str, data: &str) -> std::io::Result<()> {
    out.write_all(format!("event: {name}\nid: {id}\ndata: {data}\n\n").as_bytes())?;
    out.flush()
}

fn options<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> QueryOptions {
    QueryOptions {
        limits: None,
        timeout: ctx.query_timeout,
        cancel: None,
        database: req.param("database").map(|d| d.into_owned()),
        shards: None,
    }
}

/// Gives a subscription slot back however the stream ends, unwinding included.
struct Subscription<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for Subscription<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
