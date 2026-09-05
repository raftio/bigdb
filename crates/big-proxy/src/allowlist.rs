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

//! Every daemon route this proxy may forward, and nothing else.
//!
//! This table mirrors `resolve()` in `big-http/src/routes/mod.rs`. It is an **allowlist rather
//! than a denylist**, and the difference is the whole design: a denylist is a list somebody can
//! forget to extend, and the twenty-one `/internal/*` peer routes are excluded here simply by
//! not appearing below. A route the daemon grows tomorrow is unreachable through this proxy
//! until somebody adds it, which is the failure that gets noticed rather than the one that does
//! not.
//!
//! Matching is positional against the **decoded** segments [`big_wire::Request::segments`] hands
//! us, never against the raw request line, and the upstream path is rebuilt from the segments
//! that matched. That is what makes `..`, `%2f` and collapsed slashes structurally unable to
//! produce a request this table did not intend — not a filter that has to anticipate them.
//!
//! The order the two halves happen in is what makes the round trip exact: `segments()` splits on
//! `/` and *then* percent-decodes, so a table named `a/b` arrives as one segment and is
//! re-encoded as one. A decoder that ran first would turn that name into two segments and this
//! table would match something else.

use std::borrow::Cow;
use std::time::Duration;

/// `Duration`'s `Mul` is not a const trait, so the budgets below are spelled in seconds rather
/// than as `30 * SECOND`. `from_secs` is a `const fn`; the arithmetic around it is not.
const fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

const MINUTE: Duration = secs(60);

/// Whether a request may be sent a second time after a failure that carried no answer.
///
/// The same distinction `big_cluster::client::Repeatable` draws, and its argument is the one
/// that matters: *"Sending a fact twice writes the same bit and is invisible; sending a `delete`
/// twice reports how many records the second one removed, which is a number the caller would
/// then be told and would be wrong."*
///
/// Declared here rather than imported because importing it would mean depending on
/// `big-cluster`, and through it on the whole engine — which is the linkage `big-wire` exists to
/// avoid. `tests/agreement.rs` pins the two definitions together, the same way `tests/config.rs`
/// pins this crate's cluster-file reader to the daemon's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeatable {
    /// Safe to send again: the second answer is the first answer.
    Yes,
    /// Not safe: failing is the correct outcome.
    No,
}

/// One segment of a route pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seg {
    /// Matches this exact segment.
    Lit(&'static str),
    /// Captures exactly one segment — a table, field or database name.
    Name,
}

/// Which routes an operator has turned on.
///
/// Not a privilege check: this proxy holds no users file and verifies nothing. The daemon's
/// `Guard` is the real boundary, and this only narrows what the front door will carry to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Tier {
    /// Reads and writes. What a client is for.
    Data,
    /// Creating and dropping tables, fields and databases.
    Ddl,
    /// `/verify`, `/repair`, `/admin/*`, `/cluster/topology`.
    ///
    /// Off by default, because every one of them asks about *one node* and a proxy chooses
    /// which node without telling you. `POST /admin/backup` is the sharpest example: it copies
    /// the file of whichever node received it, so through a proxy it means "back up a node,
    /// unspecified", which is worse than not offering it.
    Ops,
}

/// One forwardable route.
#[derive(Clone, Copy, Debug)]
pub struct Route {
    pub method: &'static str,
    pub segments: &'static [Seg],
    /// Query keys forwarded upstream. Anything else is dropped, not rejected — a client that
    /// appends a parameter this table has not heard of keeps working.
    pub params: &'static [&'static str],
    /// Ceiling for one forwarded call, so a wedged socket cannot pin a worker forever.
    pub budget: Duration,
    /// Whether this may be sent to a second node after a failure that carried no answer.
    pub repeatable: Repeatable,
    pub tier: Tier,
}

/// The table, longest patterns first.
///
/// **Order is load-bearing in exactly the place it is load-bearing in `resolve()`**: the
/// two-segment `table/{t}` entries must come *after* `table/{t}/query`, `/import`, `/delete` and
/// `/field/{f}`. A `POST` to `table/x/query` is a query; only a `POST` to `table/x` creates a
/// table called `x`.
pub const ROUTES: &[Route] = &[
    // ── reads ────────────────────────────────────────────────────────────────
    Route {
        method: "GET",
        segments: &[Seg::Lit("schema")],
        params: &[],
        budget: secs(10),
        repeatable: Repeatable::Yes,
        tier: Tier::Data,
    },
    Route {
        method: "GET",
        segments: &[Seg::Lit("table"), Seg::Name, Seg::Lit("records")],
        params: &["after", "limit", "database"],
        budget: MINUTE,
        repeatable: Repeatable::Yes,
        tier: Tier::Data,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("table"), Seg::Name, Seg::Lit("query")],
        params: &["after", "limit", "database"],
        budget: MINUTE,
        repeatable: Repeatable::Yes,
        tier: Tier::Data,
    },
    // **Not repeatable, and it is the entry most likely to be argued with.** `/sql` looks like a
    // read and is not: `CREATE TABLE` goes through here too, with the statement raising the
    // privilege floor. This proxy does not parse SQL to find out which it got, and must not
    // start — retrying a `CREATE TABLE` that actually succeeded answers "table already exists"
    // for a statement that worked, which is a wrong error rather than an honest failure.
    Route {
        method: "POST",
        segments: &[Seg::Lit("sql")],
        params: &["database"],
        budget: MINUTE,
        repeatable: Repeatable::No,
        tier: Tier::Data,
    },
    // ── writes ───────────────────────────────────────────────────────────────
    Route {
        method: "POST",
        segments: &[Seg::Lit("table"), Seg::Name, Seg::Lit("import")],
        params: &["database"],
        budget: secs(120),
        repeatable: Repeatable::No,
        tier: Tier::Data,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("table"), Seg::Name, Seg::Lit("delete")],
        params: &["database"],
        budget: MINUTE,
        repeatable: Repeatable::No,
        tier: Tier::Data,
    },
    // ── DDL ──────────────────────────────────────────────────────────────────
    Route {
        method: "POST",
        segments: &[Seg::Lit("table"), Seg::Name, Seg::Lit("field"), Seg::Name],
        params: &["kind", "bit_depth", "scale", "database"],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ddl,
    },
    Route {
        method: "DELETE",
        segments: &[Seg::Lit("table"), Seg::Name, Seg::Lit("field"), Seg::Name],
        params: &["database"],
        budget: MINUTE,
        repeatable: Repeatable::No,
        tier: Tier::Ddl,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("table"), Seg::Name],
        params: &["engine", "database"],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ddl,
    },
    Route {
        method: "DELETE",
        segments: &[Seg::Lit("table"), Seg::Name],
        params: &["database"],
        budget: MINUTE,
        repeatable: Repeatable::No,
        tier: Tier::Ddl,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("database"), Seg::Name],
        params: &[],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ddl,
    },
    Route {
        method: "DELETE",
        segments: &[Seg::Lit("database"), Seg::Name],
        params: &["cascade"],
        budget: MINUTE,
        repeatable: Repeatable::No,
        tier: Tier::Ddl,
    },
    // ── ops, off unless --allow-ops ──────────────────────────────────────────
    // `/verify` is a full digest walk and `/repair` streams fragments between nodes; both are
    // minutes, not seconds.
    Route {
        method: "GET",
        segments: &[Seg::Lit("verify")],
        params: &[],
        budget: secs(120),
        repeatable: Repeatable::Yes,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("repair")],
        params: &[],
        budget: secs(900),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("backup")],
        params: &["name"],
        budget: secs(900),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    // `cluster/topology` is the one cluster route that is *not* under `/admin`, matching the
    // daemon. Every mutating one below is.
    Route {
        method: "GET",
        segments: &[Seg::Lit("cluster"), Seg::Lit("topology")],
        params: &[],
        budget: secs(10),
        repeatable: Repeatable::Yes,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("split")],
        params: &["at", "to"],
        budget: MINUTE,
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("merge")],
        params: &["range"],
        budget: MINUTE,
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("node")],
        params: &["name", "addr"],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "DELETE",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("node")],
        params: &["name"],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("admit")],
        params: &["name"],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("drain")],
        params: &["name"],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    // Moving and rebalancing copy fragments across the network.
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("move")],
        params: &["range", "to"],
        budget: secs(900),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("rebalance")],
        params: &["force"],
        budget: secs(900),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("cancel")],
        params: &["range"],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
    Route {
        method: "POST",
        segments: &[Seg::Lit("admin"), Seg::Lit("cluster"), Seg::Lit("schema-leader")],
        params: &["to"],
        budget: secs(30),
        repeatable: Repeatable::No,
        tier: Tier::Ops,
    },
];

/// A matched route and the upstream request it authorises.
#[derive(Clone, Debug)]
pub struct Match {
    pub route: &'static Route,
    /// Rebuilt from what matched, never from the raw request line.
    pub path: String,
}

impl Match {
    /// The path and query to put on the upstream request line.
    pub fn target(&self, query: &str) -> String {
        if query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{query}", self.path)
        }
    }
}

/// Whether the daemon would accept this segment as a name.
///
/// Rejecting `.` and `..` here is belt-and-braces: the upstream path is rebuilt from these
/// values, so a traversal segment could only ever produce a `404` upstream. But a name that
/// cannot be encoded is a name worth refusing before it becomes a request. Control characters
/// are refused for the same reason one layer down — a `\r` in a path is a request-splitting
/// attempt, and [`encode`] would neutralise it anyway.
///
/// **A slash is allowed, and that is a deliberate difference from the console's version of this
/// table.** The daemon's `check_name` refuses only a `.` and an over-long name, so a table
/// genuinely called `a/b` is legal and is reached as `/table/a%2Fb/query`: `segments()` splits
/// and *then* decodes, so the slash never was a separator. Since [`encode`] puts it back as
/// `%2F`, the round trip is exact and the ban would buy nothing except making a legal table
/// unreachable through this proxy. The console can afford that — an operator does not make such
/// a table — and a client-facing front door cannot.
fn is_name(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains(|c: char| c.is_control())
}

/// The first route matching this method and these decoded segments, if any.
///
/// `allowed` is the highest tier an operator has turned on. A route above it is not matched at
/// all, so it leaves through the same `404 no_such_route` an unknown path gets — deliberately
/// **not** a `403`. A distinguishable refusal would turn this table into a route-discovery
/// oracle: `POST /internal/raft` answering `403` tells a stranger the route exists.
pub fn match_route(method: &str, segments: &[Cow<'_, str>], allowed: Tier) -> Option<Match> {
    for route in ROUTES {
        if route.method != method || route.segments.len() != segments.len() {
            continue;
        }
        if route.tier > allowed {
            continue;
        }
        let matched = route.segments.iter().zip(segments).all(|(want, got)| match want {
            Seg::Lit(s) => *s == got.as_ref(),
            Seg::Name => is_name(got),
        });
        if !matched {
            continue;
        }
        let path: String =
            segments.iter().map(|s| format!("/{}", encode(s))).collect::<Vec<_>>().join("");
        return Some(Match { route, path });
    }
    None
}

/// Only the parameters this route declares, in the order it declares them.
///
/// Unknown keys are dropped rather than refused, and a key the client sent twice contributes
/// once: the daemon's own `param()` takes the first occurrence, so forwarding both would let a
/// client send one value for this table to read and a different one for the daemon to act on.
pub fn allowed_query(route: &Route, raw_query: &str) -> String {
    let mut out = String::new();
    for key in route.params {
        let Some(value) = first_param(raw_query, key) else { continue };
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(key);
        out.push('=');
        out.push_str(&encode(&value));
    }
    out
}

/// The first value for `key` in a raw query string, percent-decoded.
fn first_param(raw_query: &str, key: &str) -> Option<String> {
    raw_query.split('&').filter(|p| !p.is_empty()).find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(k) == key).then(|| decode(v))
    })
}

/// Percent-decode, treating `+` as a space the way a query string does.
pub(crate) fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match hex(bytes[i + 1]).zip(hex(bytes[i + 2])) {
                Some((hi, lo)) => {
                    out.push(hi << 4 | lo);
                    i += 3;
                }
                None => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode everything outside the unreserved set.
///
/// Deliberately conservative — `/`, `?`, `#`, `&`, `=` and `%` all become escapes. This runs on
/// values that came out of a decoded segment, so encoding more than strictly necessary costs a
/// few bytes and guarantees the value cannot leave the position it was matched in.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(path: &str) -> Vec<Cow<'static, str>> {
        path.split('/').filter(|s| !s.is_empty()).map(|s| Cow::Owned(decode(s))).collect()
    }

    fn m(method: &str, path: &str) -> Option<Match> {
        match_route(method, &segs(path), Tier::Ops)
    }

    #[test]
    fn forwards_a_query() {
        let hit = m("POST", "/table/events/query").expect("a query is forwardable");
        assert_eq!(hit.path, "/table/events/query");
        assert_eq!(hit.route.repeatable, Repeatable::Yes);
    }

    /// The ordering trap: a two-segment `table/{t}` entry placed first would swallow this.
    #[test]
    fn query_is_not_a_create_table() {
        let hit = m("POST", "/table/x/query").expect("a query is forwardable");
        assert_eq!(hit.route.segments.len(), 3, "matched table/{{t}} instead of table/{{t}}/query");
        assert_eq!(hit.route.tier, Tier::Data);
    }

    #[test]
    fn create_table_is_still_reachable() {
        let hit = m("POST", "/table/x").expect("creating a table is forwardable");
        assert_eq!(hit.route.tier, Tier::Ddl);
        assert_eq!(hit.route.repeatable, Repeatable::No);
    }

    /// The one rule that closes the peer surface, and the reason this is an allowlist.
    #[test]
    fn no_internal_route_is_forwardable() {
        for path in [
            "/internal/query",
            "/internal/raft",
            "/internal/fragment/put",
            "/internal/keys/put",
            "/internal/floors/put",
            "/internal/import",
            "/internal/ddl",
        ] {
            assert!(m("POST", path).is_none(), "{path} must not be forwardable");
        }
    }

    #[test]
    fn sql_is_never_repeatable() {
        // It carries CREATE TABLE as well as SELECT, and this proxy does not parse SQL to tell.
        assert_eq!(m("POST", "/sql").unwrap().route.repeatable, Repeatable::No);
    }

    #[test]
    fn ops_routes_are_off_by_default() {
        let ops = ["/verify", "/cluster/topology"];
        for path in ops {
            assert!(match_route("GET", &segs(path), Tier::Data).is_none(), "{path} leaked");
            assert!(match_route("GET", &segs(path), Tier::Ddl).is_none(), "{path} leaked");
            assert!(match_route("GET", &segs(path), Tier::Ops).is_some(), "{path} unreachable");
        }
    }

    #[test]
    fn ddl_can_be_narrowed_away() {
        assert!(match_route("DELETE", &segs("/table/x"), Tier::Data).is_none());
        assert!(match_route("DELETE", &segs("/table/x"), Tier::Ddl).is_some());
    }

    /// Health, readiness and metrics are answered by this proxy, so they are not in the table.
    #[test]
    fn probes_are_not_forwarded() {
        assert!(m("GET", "/health").is_none());
        assert!(m("GET", "/ready").is_none());
        assert!(m("GET", "/metrics").is_none());
    }

    #[test]
    fn traversal_segments_are_refused() {
        assert!(m("POST", "/table/../query").is_none());
        assert!(m("POST", "/table/./query").is_none());
        assert!(m("DELETE", "/table/..").is_none());
    }

    /// `%2f` stays inside one segment, so the name round-trips instead of becoming a path.
    ///
    /// `check_name` in the catalog refuses only a `.` and an over-long name, so `a/b` is a legal
    /// table and has to stay reachable. What makes that safe is the re-encoding, not a ban.
    #[test]
    fn an_encoded_slash_stays_one_segment() {
        let hit = m("POST", "/table/a%2Fb/query").expect("a slash in a name is a name");
        assert_eq!(hit.path, "/table/a%2Fb/query");
        assert_eq!(hit.route.segments.len(), 3, "the slash must not have become a separator");
    }

    /// The traversal case and the slash case are different, and only one is refused.
    #[test]
    fn a_slash_in_a_name_cannot_escape_its_position() {
        // `..%2F..%2Fetc` decodes to `../../etc`, which is a name this table has no reason to
        // like — but it cannot leave the segment it matched, because the path is rebuilt.
        let hit = m("POST", "/table/..%2F..%2Fetc/query").expect("still one segment");
        assert_eq!(hit.path, "/table/..%2F..%2Fetc/query");
        assert!(!hit.path.contains("/../"), "a rebuilt path never contains a live traversal");
    }

    /// Collapsed slashes cannot smuggle an empty segment past a positional match.
    #[test]
    fn empty_segments_are_dropped_not_matched() {
        let hit = m("POST", "//table//x//query").expect("empty segments are not segments");
        assert_eq!(hit.path, "/table/x/query");
    }

    #[test]
    fn control_characters_are_refused() {
        assert!(m("POST", "/table/a%0Db/query").is_none());
        assert!(m("POST", "/table/a%00b/query").is_none());
    }

    #[test]
    fn unknown_parameters_are_dropped_not_rejected() {
        let hit = m("POST", "/table/x").unwrap();
        assert_eq!(
            allowed_query(hit.route, "evil=1&engine=bitmap&database=d"),
            "engine=bitmap&database=d"
        );
    }

    #[test]
    fn parameters_come_back_in_the_order_the_route_declares() {
        let hit = m("GET", "/table/x/records").unwrap();
        assert_eq!(allowed_query(hit.route, "limit=10&after=5"), "after=5&limit=10");
    }

    #[test]
    fn a_repeated_parameter_contributes_once() {
        // The daemon's `param()` takes the first occurrence; forwarding both would let a client
        // show this table one value and the daemon another.
        let hit = m("GET", "/table/x/records").unwrap();
        assert_eq!(allowed_query(hit.route, "limit=1&limit=9999"), "limit=1");
    }

    #[test]
    fn a_method_the_route_does_not_have_does_not_match() {
        assert!(m("DELETE", "/sql").is_none());
        assert!(m("GET", "/table/x/query").is_none());
        assert!(m("PUT", "/table/x").is_none());
    }

    #[test]
    fn every_route_matches_its_own_canonical_path() {
        for route in ROUTES {
            let path: String = route
                .segments
                .iter()
                .map(|s| match s {
                    Seg::Lit(l) => format!("/{l}"),
                    Seg::Name => "/n".to_string(),
                })
                .collect();
            let hit = match_route(route.method, &segs(&path), Tier::Ops)
                .unwrap_or_else(|| panic!("{} {path} matches nothing", route.method));
            assert_eq!(hit.path, path, "{} {path} rebuilt as {}", route.method, hit.path);
        }
    }
}
