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

//! Two nodes, two loopback ports, and nothing mocked.
//!
//! Every request here goes out over TCP, is planned on one machine, executed on two and merged
//! back. The point of doing it this way rather than against a `Cluster` in one process is that
//! the encoding, the fan-out, the row-key agreement and the merge are each capable of being
//! individually right and collectively wrong, and only a real socket exercises all four.
//!
//! Records below `SHARD_WIDTH` belong to `a` and everything above to `b`, so a batch that
//! crosses that line is a batch that crosses machines.

use big_cluster::controller::Leases;
use big_cluster::raft::{Forgetful, Timing};
use big_cluster::{Cluster, ClusterFile};
use big_embed::Api;
use big_http::{Server, ServerConfig};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One shard's worth of record ids. A record id names its shard, so this is also the line
/// between the two nodes.
const WIDTH: u64 = 1 << 20;

/// A port nothing is listening on yet.
///
/// Bound and released rather than guessed. The window between releasing and re-binding is a
/// race in principle; a listener that never accepted anything leaves no `TIME_WAIT` behind it,
/// so in practice the port is free and the alternative is a hard-coded number that collides
/// with whatever else is on the machine.
fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

/// A two-node cluster on loopback, `a` owning shard 0 and leading the schema.
fn two_nodes() -> (SocketAddr, SocketAddr) {
    let (a, b) = (free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..1\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"1..\"\n"
    );
    start(&file, &[("a", a), ("b", b)]);
    (a, b)
}

/// Two nodes that know what their cluster is called, which is what a node with no file has to
/// be told before it can ask anything.
fn two_named_nodes(id: &str) -> (SocketAddr, SocketAddr) {
    let (a, b) = (free_port(), free_port());
    let file = format!(
        "cluster_id = \"{id}\"\nschema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..1\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"1..\"\n"
    );
    start(&file, &[("a", a), ("b", b)]);
    (a, b)
}

/// A named cluster that runs an agreement: one primary and two copies, which is the smallest
/// shape that can commit anything and therefore the smallest that can admit a node.
fn a_named_replicated_group(id: &str) -> (SocketAddr, SocketAddr, SocketAddr) {
    let (a, spare, third) = (free_port(), free_port(), free_port());
    let file = format!(
        "cluster_id = \"{id}\"\nschema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n\
         [[node]]\nname = \"a-third\"\naddr = \"{third}\"\nreplica = \"a\"\n"
    );
    start(&file, &[("a", a), ("a-spare", spare), ("a-third", third)]);
    waiting("an elected leader", std::time::Duration::from_secs(15), || {
        ready(a).contains(r#""leader":""#)
    });
    (a, spare, third)
}

/// The fingerprint of the file the harness last started, for a cluster that has no name and is
/// therefore known by its shape.
fn shape_fingerprint() -> u64 {
    LAST_FILE.with(|f| {
        ClusterFile::parse(&f.borrow()).unwrap().for_node(Some("a"), "").unwrap().fingerprint()
    })
}

/// The fingerprint of the file [`a_replicated_group`] writes, for a test that has to send a
/// request the way a peer would.
fn a_replicated_group_fingerprint() -> u64 {
    LAST_FILE.with(|f| {
        ClusterFile::parse(&f.borrow()).unwrap().for_node(Some("a"), "").unwrap().fingerprint()
    })
}

thread_local! {
    /// The cluster file the harness most recently started. Kept so a test can compute the
    /// fingerprint the nodes are using without the fixture having to hand it back through
    /// every caller.
    static LAST_FILE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// One primary and two copies of it, over the whole shard space.
///
/// Three rather than two, and not for symmetry: failing over is a decision a majority has to
/// agree on, and a majority of two is two - so a cluster of two can never use its copy, and
/// the file is refused for saying so.
fn a_replicated_group() -> (SocketAddr, SocketAddr, SocketAddr) {
    let (a, spare, third) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n\
         [[node]]\nname = \"a-third\"\naddr = \"{third}\"\nreplica = \"a\"\n"
    );
    start(&file, &[("a", a), ("a-spare", spare), ("a-third", third)]);
    // **Wait for the agreement before asking anything of it.** Every node here holds a
    // replicated range, so every one of them is fenced: until it has heard from the agreement
    // it cannot know it has not already been replaced, and it refuses rather than risk being
    // the second node answering for one range. On the shipped clocks that is an election, and
    // an election is what a replicated cluster needs before it can serve at all.
    waiting("an elected leader", std::time::Duration::from_secs(15), || {
        ready(a).contains(r#""leader":""#)
    });
    (a, spare, third)
}

/// Starts one node per name, each on its own in-memory database, all reading one config file.
///
/// A name may be left out, which is how a test has a node that is configured and not running.
fn start(file: &str, names: &[(&str, SocketAddr)]) {
    LAST_FILE.with(|f| *f.borrow_mut() = file.to_string());
    for (name, addr) in names {
        let config = ClusterFile::parse(file).unwrap().for_node(Some(name), "").unwrap();
        let cluster =
            Cluster::new(Api::in_memory().unwrap(), config, None, Box::new(Forgetful)).unwrap();
        let server = Server::bind_cluster(cluster, *addr, ServerConfig::default())
            .expect("the port was free");
        std::thread::spawn(move || {
            let _ = server.serve();
        });
    }
    // Every listener is bound by the time `bind_cluster` returned, so a request sent now is
    // accepted even if no worker has reached the queue yet.
}

/// Sends a request and returns `(status, body)`.
fn send(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status in {raw:?}"));
    (status, raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string())
}

fn ok(addr: SocketAddr, method: &str, target: &str, body: &str) -> String {
    let (status, out) = send(addr, method, target, body);
    assert_eq!(status, 200, "{method} {target}: {out}");
    out
}

/// Schema on one node, facts on both, answers merged.
///
/// The `Sum` and the `Count` are the whole shape of the thing: neither node holds the answer,
/// and neither knows that.
#[test]
fn a_query_is_answered_by_both_nodes_and_merged() {
    let (a, b) = two_nodes();

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");

    // A schema change goes to the leader first and then everywhere else, so the node that
    // never saw the request has the table.
    let schema = ok(b, "GET", "/schema", "");
    assert!(schema.contains(r#""name":"tx""#), "{schema}");
    assert!(schema.contains(r#""name":"country""#), "{schema}");

    // Records 1 and 2 belong to `a`; the two above `WIDTH` belong to `b`.
    let facts = format!(
        "amount 1 100\ncountry 1 GB\n\
         amount 2 900\ncountry 2 US\n\
         amount {r3} 300\ncountry {r3} GB\n\
         amount {r4} 700\ncountry {r4} FR\n",
        r3 = WIDTH + 1,
        r4 = WIDTH + 2,
    );
    assert_eq!(ok(a, "POST", "/table/tx/import", &facts), r#"{"imported":8}"#);

    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":4}"#);
    assert_eq!(
        ok(a, "POST", "/table/tx/query", r#"Sum(All(), field="amount")"#),
        r#"{"sum":2000}"#
    );
    // A key written on both nodes is one row on both nodes, which is the schema leader's
    // entire job. If it were not, this would answer 1.
    assert_eq!(ok(a, "POST", "/table/tx/query", r#"Count(Row(country="GB"))"#), r#"{"count":2}"#);

    // Any node is a coordinator. The one that owns none of the GB records answers the same.
    assert_eq!(ok(b, "POST", "/table/tx/query", r#"Count(Row(country="GB"))"#), r#"{"count":2}"#);
    assert_eq!(ok(b, "POST", "/table/tx/query", "Count(All())"), r#"{"count":4}"#);

    // A set of records crosses the wire as containers and comes back in order.
    let rows = ok(b, "POST", "/table/tx/query", r#"Row(country="GB")"#);
    assert_eq!(rows, format!("{{\"records\":[1,{}],\"next\":null}}", WIDTH + 1));

    assert_eq!(
        ok(a, "POST", "/table/tx/query", r#"Min(Row(country="GB"), field="amount")"#),
        r#"{"value":100}"#
    );
    assert_eq!(
        ok(a, "POST", "/table/tx/query", r#"Max(All(), field="amount")"#),
        r#"{"value":900}"#
    );
}

/// The ranking one. A group that leads no single node still wins the cluster.
///
/// `US` leads node `a` with three and holds nothing on `b`. `GB` has two on each. A `TopN`
/// that truncated at the owners would answer `US`; the answer is `GB`, because every node's
/// contribution to a group is summed before anything is ranked or cut.
#[test]
fn top_n_ranks_after_every_node_has_contributed() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");

    let mut facts = String::new();
    for i in 0..3 {
        facts.push_str(&format!("country {} US\n", i + 1));
    }
    for i in 0..2 {
        facts.push_str(&format!("country {} GB\n", 100 + i));
        facts.push_str(&format!("country {} GB\n", WIDTH + 100 + i));
    }
    ok(a, "POST", "/table/tx/import", &facts);

    let top = ok(b, "POST", "/table/tx/query", r#"TopN(All(), field="country", n=1)"#);
    assert_eq!(top, r#"{"groups":[{"key":"GB","row":0,"value":{"count":4}}]}"#, "{top}");

    // The same sum, seen through the aggregate that does not rank.
    let all = ok(a, "POST", "/table/tx/query", r#"Distinct(All(), field="country")"#);
    assert_eq!(
        all,
        r#"{"groups":[{"key":"GB","row":0,"value":{"count":4}},{"key":"US","row":1,"value":{"count":3}}]}"#,
        "{all}"
    );
}

/// The same questions in SQL, across two nodes, answered identically to their PQL twins.
///
/// **`count(DISTINCT country)` is the one that would catch the bug this design exists to
/// avoid.** Its plan is a `Distinct`, and the counting is not part of the plan - it is the
/// shape, applied by the coordinator *after* the merge. Counted at each owner and summed, the
/// answer below would be 4: two countries on `a` and two on `b`, with `GB` double-counted
/// because both hold some of it. It is 3, because the merge folds a group that two nodes both
/// hold into one group before anything counts them.
///
/// Nothing else here is new machinery, and that is the point being asserted: `Cluster::sql`
/// plans locally and hands the plan to the fan-out that already existed, so a SQL statement is
/// merged by the code that merges PQL.
#[test]
fn sql_is_answered_across_nodes_and_merged() {
    let (a, b) = two_nodes();

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");

    let facts = format!(
        "amount 1 100\ncountry 1 GB\n\
         amount 2 900\ncountry 2 US\n\
         amount {r3} 300\ncountry {r3} GB\n\
         amount {r4} 700\ncountry {r4} FR\n",
        r3 = WIDTH + 1,
        r4 = WIDTH + 2,
    );
    assert_eq!(ok(a, "POST", "/table/tx/import", &facts), r#"{"imported":8}"#);

    // Scalars, from the node that owns half the records and from the node that owns the rest.
    assert_eq!(
        ok(a, "POST", "/sql", "SELECT count(*) FROM tx"),
        r#"{"columns":["count"],"rows":[[4]]}"#
    );
    assert_eq!(
        ok(b, "POST", "/sql", "SELECT sum(amount) FROM tx"),
        r#"{"columns":["sum"],"rows":[[2000]]}"#
    );
    assert_eq!(
        ok(b, "POST", "/sql", "SELECT count(*) FROM tx WHERE country = 'GB'"),
        r#"{"columns":["count"],"rows":[[2]]}"#
    );
    assert_eq!(
        ok(a, "POST", "/sql", "SELECT max(amount) FROM tx"),
        r#"{"columns":["max"],"rows":[[900]]}"#
    );

    // Groups, merged across owners before they are rendered.
    assert_eq!(
        ok(a, "POST", "/sql", "SELECT country, count(*) FROM tx GROUP BY country"),
        r#"{"columns":["country","count"],"rows":[["FR",1],["GB",2],["US",1]]}"#
    );

    // Three countries, not four. See this test's header.
    assert_eq!(
        ok(b, "POST", "/sql", "SELECT count(DISTINCT country) FROM tx"),
        r#"{"columns":["count"],"rows":[[3]]}"#
    );

    // `SELECT *` reads every column back on each owner, and the rows interleave by record id
    // when the two answers are merged - record 1 lives on one node and `WIDTH + 1` on the other.
    assert_eq!(
        ok(b, "POST", "/sql", "SELECT * FROM tx WHERE country = 'GB'"),
        r#"{"columns":["amount","country"],"rows":[[100,"GB"],[300,"GB"]]}"#
    );
}

/// A ranking, in SQL, over a group that leads no single node.
///
/// The PQL twin of this is `top_n_ranks_after_every_node_has_contributed`. Both statements plan
/// to the same `TopN`, so both are cut only after every node's contribution to each group has
/// been summed - which is what makes the answer `GB` rather than the `US` that leads node `a`.
#[test]
fn a_sql_ranking_is_cut_after_the_merge_not_at_the_owners() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");

    let mut facts = String::new();
    for i in 0..3 {
        facts.push_str(&format!("country {} US\n", i + 1));
    }
    for i in 0..2 {
        facts.push_str(&format!("country {} GB\n", 100 + i));
        facts.push_str(&format!("country {} GB\n", WIDTH + 100 + i));
    }
    ok(a, "POST", "/table/tx/import", &facts);

    let top = ok(
        b,
        "POST",
        "/sql",
        "SELECT country, count(*) AS n FROM tx GROUP BY country ORDER BY n DESC LIMIT 1",
    );
    assert_eq!(top, r#"{"columns":["country","n"],"rows":[["GB",4]]}"#, "{top}");
}

/// A projection across two nodes, merged into one page.
///
/// **This is the test the new merge arm exists for.** Each owner is asked for the whole page,
/// because the first `n` records overall can all live on one node - so `a` answers with its
/// records and `b` with its own, and neither of them is the answer. The coordinator interleaves
/// them by record id and cuts once. A merge that concatenated instead would answer with `a`'s
/// records followed by `b`'s, which for `LIMIT 3` below would be the wrong three rows in the
/// wrong order, and nothing in the result set could show it.
#[test]
fn a_projection_is_merged_across_owners_and_cut_once() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");

    // Two records on each node, interleaved by id so that neither node's own answer is a
    // prefix of the right one.
    let facts = format!(
        "amount 1 100\namount {r2} 200\namount 3 300\namount {r4} 400\n",
        r2 = WIDTH + 2,
        r4 = WIDTH + 4,
    );
    assert_eq!(ok(a, "POST", "/table/tx/import", &facts), r#"{"imported":4}"#);

    // The whole page, in record order, across both owners.
    assert_eq!(
        ok(b, "POST", "/sql", "SELECT amount FROM tx LIMIT 10"),
        r#"{"columns":["amount"],"rows":[[100],[300],[200],[400]]}"#
    );

    // Cut to three. `a` holds records 1 and 3, `b` holds the other two: the right answer takes
    // two rows from one node and one from the other, which only the merge can do.
    assert_eq!(
        ok(b, "POST", "/sql", "SELECT amount FROM tx LIMIT 3"),
        r#"{"columns":["amount"],"rows":[[100],[300],[200]]}"#
    );

    // And narrowed, so the records read are only the ones that matched.
    assert_eq!(
        ok(b, "POST", "/sql", "SELECT amount FROM tx WHERE amount >= 300 LIMIT 10"),
        r#"{"columns":["amount"],"rows":[[300],[400]]}"#
    );
}

/// A join across two nodes, paired after both sides have been merged.
///
/// **This is the test the whole design of joins rests on.** `GB` holds two orders on node `a`
/// and one on `b`, and two shops split one apiece. The join has `(2+1) · (1+1) = 6` rows.
/// Multiplying at each owner and summing the products would give `2·1 + 1·1 = 3` — a plausible
/// number, off by half, that no client could tell from the right one. The multiplication
/// therefore cannot happen anywhere but at the coordinator, after each side's per-key counts
/// have been summed across every node holding them, which is why a join is a `Shape` and not a
/// `Plan`.
#[test]
fn a_join_multiplies_after_the_merge_not_at_the_owners() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/orders", "");
    ok(a, "POST", "/table/orders/field/country?kind=set", "");
    ok(a, "POST", "/table/orders/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/shops", "");
    ok(a, "POST", "/table/shops/field/country?kind=set", "");

    // Two orders on `a`, one on `b`; one shop on each.
    let orders = format!(
        "country 1 GB\namount 1 100\ncountry 2 GB\namount 2 200\n\
         country {r} GB\namount {r} 300\n",
        r = WIDTH + 1,
    );
    assert_eq!(ok(a, "POST", "/table/orders/import", &orders), r#"{"imported":6}"#);
    let shops = format!("country 1 GB\ncountry {} GB\n", WIDTH + 1);
    assert_eq!(ok(a, "POST", "/table/shops/import", &shops), r#"{"imported":2}"#);

    // Six, not three.
    assert_eq!(
        ok(
            b,
            "POST",
            "/sql",
            "SELECT count(*) FROM orders o JOIN shops s ON o.country = s.country"
        ),
        r#"{"columns":["count"],"rows":[[6]]}"#
    );

    // The same for a total: 600 across both nodes, seen once per shop.
    assert_eq!(
        ok(
            a,
            "POST",
            "/sql",
            "SELECT sum(o.amount) FROM orders o JOIN shops s ON o.country = s.country"
        ),
        r#"{"columns":["sum"],"rows":[[1200]]}"#
    );

    // And the key is one key, not one per node that holds it.
    assert_eq!(
        ok(
            b,
            "POST",
            "/sql",
            "SELECT o.country, count(*) FROM orders o JOIN shops s ON o.country = s.country \
             GROUP BY o.country"
        ),
        r#"{"columns":["country","count"],"rows":[["GB",6]]}"#
    );
}

/// A pair grouping across two nodes, folded on the pair before anything is ordered.
///
/// **The pair split across both nodes is the test.** `GB/a` holds two records on `a` and one on
/// `b`; merging by concatenation would answer with two rows for it, and merging by the left key
/// alone would fuse `GB/a` with `GB/b`. It is one row, and it counts three.
#[test]
fn a_pair_grouping_is_folded_across_owners() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");
    ok(a, "POST", "/table/tx/field/category?kind=set", "");

    let facts = format!(
        "country 1 GB\ncategory 1 a\ncountry 2 GB\ncategory 2 a\n\
         country {r3} GB\ncategory {r3} a\ncountry {r4} GB\ncategory {r4} b\n\
         country {r5} US\ncategory {r5} a\n",
        r3 = WIDTH + 1,
        r4 = WIDTH + 2,
        r5 = WIDTH + 3,
    );
    assert_eq!(ok(a, "POST", "/table/tx/import", &facts), r#"{"imported":10}"#);

    // GB/a is three: two from `a`, one from `b`.
    assert_eq!(
        ok(
            b,
            "POST",
            "/sql",
            "SELECT country, category, count(*) FROM tx GROUP BY country, category"
        ),
        r#"{"columns":["country","category","count"],"rows":[["GB","a",3],["GB","b",1],["US","a",1]]}"#
    );

    // And the pair counts still sum to the table.
    assert_eq!(
        ok(a, "POST", "/sql", "SELECT count(*) FROM tx"),
        r#"{"columns":["count"],"rows":[[5]]}"#
    );
}

/// A quantile across two nodes, where the bound moves on the merged count.
///
/// **A quantile is not mergeable from per-node quantiles**, and this is the test that says so.
/// Node `a` holds 10, 20 and 30; node `b` holds 40 and 50. Their medians are 20 and 40, and
/// neither of those - nor anything derived from the pair alone - is the answer. The median of
/// all five is 30, which only a search over the merged counts finds.
#[test]
fn a_quantile_moves_its_bound_on_the_merged_count() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");

    let facts = format!(
        "amount 1 10\namount 2 20\namount 3 30\namount {r4} 40\namount {r5} 50\n",
        r4 = WIDTH + 1,
        r5 = WIDTH + 2,
    );
    assert_eq!(ok(a, "POST", "/table/tx/import", &facts), r#"{"imported":5}"#);

    for node in [a, b] {
        assert_eq!(
            ok(node, "POST", "/sql", "SELECT median(amount) FROM tx"),
            r#"{"columns":["quantile"],"rows":[[30]]}"#
        );
    }

    // The ends, which are the min and the max of everything rather than of one node.
    assert_eq!(
        ok(b, "POST", "/sql", "SELECT quantile(0)(amount), quantile(1)(amount) FROM tx"),
        r#"{"columns":["quantile","quantile"],"rows":[[10,50]]}"#
    );
}

/// A refusal is refused by the coordinator, before any node is asked.
///
/// Planning is pure, so a statement that cannot be answered never reaches the network - the
/// same property `Cluster::query` relies on, and the reason a bad statement costs a round trip
/// to nobody.
#[test]
fn a_refused_statement_never_reaches_a_peer() {
    let (a, _b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/k?kind=set", "");

    let (status, body) = send(a, "POST", "/sql", "SELECT count(*) FROM tx, t2");
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_no_joins""#), "{body}");

    // And a join that *is* answered still refuses before the network when its second table is
    // not there: planning resolves every call, so the statement fails whole.
    let (status, body) =
        send(a, "POST", "/sql", "SELECT count(*) FROM tx t JOIN nope n ON t.k = n.k");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains(r#""code":"unknown_table""#), "{body}");
}

/// A listing walks the whole space in order, one page at a time, across both nodes.
#[test]
fn records_are_listed_and_paged_across_nodes() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");

    let ids = [1u64, 2, WIDTH, WIDTH + 5];
    let facts: String = ids.iter().map(|i| format!("amount {i} 1\n")).collect();
    ok(a, "POST", "/table/tx/import", &facts);

    let all = ok(b, "GET", "/table/tx/records", "");
    assert_eq!(all, format!("{{\"records\":[1,2,{},{}],\"next\":null}}", WIDTH, WIDTH + 5));

    // A page that ends inside the first node's range, and the page after it, which starts in
    // the first node's range and finishes in the second's.
    let first = ok(a, "GET", "/table/tx/records?limit=2", "");
    assert_eq!(first, r#"{"records":[1,2],"next":2}"#);
    let second = ok(a, "GET", "/table/tx/records?after=2&limit=2", "");
    assert_eq!(second, format!("{{\"records\":[{},{}],\"next\":{}}}", WIDTH, WIDTH + 5, WIDTH + 5));
    let third = ok(a, "GET", &format!("/table/tx/records?after={}&limit=2", WIDTH + 5), "");
    assert_eq!(third, r#"{"records":[],"next":null}"#);
}

/// A delete is split by owner like an import, and answers with the total it removed.
#[test]
fn a_delete_reaches_the_node_that_holds_the_record() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", &format!("amount 1 1\namount {} 1\n", WIDTH + 3));

    assert_eq!(ok(b, "POST", "/table/tx/query", "Count(All())"), r#"{"count":2}"#);
    assert_eq!(
        ok(b, "POST", "/table/tx/delete", &format!("1\n{}\n", WIDTH + 3)),
        r#"{"deleted":2}"#
    );
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":0}"#);
}

/// Dropping schema reaches every node, not only the leader.
#[test]
fn dropping_a_table_reaches_every_node() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(b, "DELETE", "/table/tx", "");

    for node in [a, b] {
        let schema = ok(node, "GET", "/schema", "");
        assert_eq!(schema, r#"{"tables":[]}"#, "{schema}");
    }
}

/// **The whole query fails.** A count that is missing a node's contribution looks exactly like
/// a correct count, and there is no downstream check that would catch it - so an owner that
/// cannot be reached is a `503` naming the shards, never a partial answer.
#[test]
fn an_unreachable_owner_fails_the_whole_query() {
    let a = free_port();
    // Nothing is ever started here. A node that is down at startup is not a configuration
    // error; it is a node that is down.
    let gone = free_port();
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..1\"\n\
         [[node]]\nname = \"gone\"\naddr = \"{gone}\"\nshards = \"1..\"\n"
    );
    let config = ClusterFile::parse(&file).unwrap().for_node(Some("a"), "").unwrap();
    let cluster =
        Cluster::new(Api::in_memory().unwrap(), config, None, Box::new(Forgetful)).unwrap();
    let server = Server::bind_cluster(cluster, a, ServerConfig::default()).unwrap();
    std::thread::spawn(move || {
        let _ = server.serve();
    });

    // The schema leader is this node, so a table can still be created - and the peer that
    // could not be told about it is named, rather than the change being reported as done.
    let (status, body) = send(a, "POST", "/table/tx", "");
    assert_eq!(status, 500, "{body}");
    assert!(body.contains(r#""code":"partially_applied""#), "{body}");
    assert!(body.contains("gone"), "{body}");

    let (status, body) = send(a, "POST", "/table/tx/query", "Count(All())");
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(r#""code":"owner_unreachable""#), "{body}");
    // The shard range is the part an operator cannot work out from a 503 on its own.
    assert!(body.contains("1.."), "{body}");
}

/// A node that is not the leader cannot invent a row id, and says so rather than assigning one
/// locally and reconciling later. Two row ids for one string is the failure that has no
/// downstream symptom.
#[test]
fn a_new_key_is_refused_when_the_leader_is_unreachable() {
    let b = free_port();
    let gone = free_port();
    let file = format!(
        "schema_leader = \"gone\"\n\
         [[node]]\nname = \"gone\"\naddr = \"{gone}\"\nshards = \"0..1\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"1..\"\n"
    );
    let config = ClusterFile::parse(&file).unwrap().for_node(Some("b"), "").unwrap();
    let api = Api::in_memory().unwrap();
    // The schema exists locally: this test is about the row key, not about the table.
    api.create_table("tx").unwrap();
    api.create_field("tx", "country", big_embed::FieldKind::Set, 0).unwrap();
    let cluster = Cluster::new(api, config, None, Box::new(Forgetful)).unwrap();
    let server = Server::bind_cluster(cluster, b, ServerConfig::default()).unwrap();
    std::thread::spawn(move || {
        let _ = server.serve();
    });

    let (status, body) =
        send(b, "POST", "/table/tx/import", &format!("country {} GB\n", WIDTH + 1));
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(r#""code":"schema_leader_unreachable""#), "{body}");

    // Reads are unaffected by the leader being gone, as long as no owner is.
    let (status, body) = send(b, "POST", "/table/tx/query", r#"Count(Row(country="GB"))"#);
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(r#""code":"owner_unreachable""#), "{body}");
}

/// Readiness is about this node, not about the cluster. A node whose peer is down is still
/// able to serve its own shards, and a probe that failed for somebody else's outage would take
/// a healthy node out of rotation.
#[test]
fn readiness_reports_this_node_and_ignores_its_peers() {
    let (a, _b) = two_nodes();
    let body = ok(a, "GET", "/ready", "");
    assert!(body.contains(r#""node":"a""#), "{body}");
    assert!(body.contains(r#""shards":"0..1""#), "{body}");
}

/// The fan-out reuses its connections.
///
/// Twenty queries through `a`, each one reaching `b`, and `b` should have accepted far fewer
/// than twenty connections. This is the only place the reuse is observable from outside: the
/// answers are identical either way, and the difference is a handshake per leg.
#[test]
fn the_fan_out_reuses_its_connections() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");

    const QUERIES: u64 = 20;
    for _ in 0..QUERIES {
        ok(a, "POST", "/table/tx/query", "Count(All())");
    }

    // Scraped over a connection of its own, which is one of the ones being counted.
    let metrics = ok(b, "GET", "/metrics", "");
    let accepted = metrics
        .lines()
        .find_map(|l| l.strip_prefix("big_http_connections_accepted_total "))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or_else(|| panic!("no connection counter in {metrics}"));

    // Not `== 1`: the schema changes reached `b` first, and this scrape is a connection too.
    // What matters is that twenty queries did not cost twenty connections.
    // Three would be the honest floor: the two schema changes and this scrape. A little slack
    // for a connection the pool happened to find closed, and nothing like twenty.
    assert!(accepted <= 6, "{accepted} connections for {QUERIES} queries:\n{metrics}");
}

// -------------------------------------------------------------------------------------------
// Replication
// -------------------------------------------------------------------------------------------

/// A write reaches both copies, and the copy is a real database rather than a log of
/// intentions: the spare answers the same query with the same numbers.
#[test]
fn a_write_reaches_every_copy_of_a_range() {
    let (a, spare, _third) = a_replicated_group();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");
    ok(a, "POST", "/table/tx/import", "amount 1 100\ncountry 1 GB\namount 2 900\ncountry 2 US\n");

    // The schema reached the spare too: it is a database, not a queue of intentions.
    for node in [a, spare] {
        let schema = ok(node, "GET", "/schema", "");
        assert!(schema.contains(r#""name":"country""#), "{schema}");
    }

    // The digests are what actually compares the two, field by field and row by row.
    let report = ok(a, "GET", "/verify", "");
    assert!(report.contains(r#""agree":true"#), "{report}");
    assert!(report.contains(r#""node":"a-spare""#), "{report}");
    assert!(!report.contains(r#""digest":null"#), "{report}");
}

/// **Reads go to the primary, never to a replica.** A replica holds the same records, so
/// asking both would double every count - and asking the replica *instead* would answer from a
/// copy this node cannot know is current.
#[test]
fn a_replica_does_not_double_a_count() {
    let (a, spare, _third) = a_replicated_group();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", "amount 1 5\namount 2 5\n");

    for node in [a, spare] {
        assert_eq!(ok(node, "POST", "/table/tx/query", "Count(All())"), r#"{"count":2}"#);
        assert_eq!(
            ok(node, "POST", "/table/tx/query", r#"Sum(All(), field="amount")"#),
            r#"{"sum":10}"#
        );
    }
}

/// **A copy that is down does not fail the write**, and the answer says which copy is behind.
///
/// This is the hole automatic failover does not plug on its own: failover replaces a dead node
/// that reads go to, and this is a dead *spare*. Refusing the write would mean one machine
/// nobody reads from can stop the whole range being written to.
///
/// What makes it safe is the other half: the agreement marks that copy behind, and a copy
/// marked behind is one it will not promote. Nothing ever reads from a copy that missed a
/// write.
#[test]
fn a_write_that_misses_a_copy_stands_and_says_so() {
    let (a, spare, third) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n\
         [[node]]\nname = \"a-third\"\naddr = \"{third}\"\nreplica = \"a\"\n"
    );
    // `a-third` is configured and never started, so a majority is still two of three and the
    // agreement works - it is one copy that is missing, not the quorum.
    start(&file, &[("a", a), ("a-spare", spare)]);
    waiting("an elected leader", std::time::Duration::from_secs(15), || {
        ready(a).contains(r#""leader":""#)
    });

    // A schema change reaches the copies it can and names the one it cannot.
    let (status, body) = send(a, "POST", "/table/tx", "");
    assert_eq!(status, 500, "{body}");
    assert!(body.contains(r#""code":"partially_applied""#), "{body}");
    let (status, body) = send(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    assert_eq!(status, 500, "{body}");

    // The write stands. It landed on the copy reads go to and on one spare, and the answer
    // names the copy it did not reach.
    let (status, body) = send(a, "POST", "/table/tx/import", "amount 1 5\n");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""imported":1"#), "{body}");
    assert!(body.contains("a-third"), "{body}");

    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":1}"#);

    // And `verify` says what an operator needs before promoting anything: one copy could not
    // be asked, so nothing here is agreement.
    let report = ok(a, "GET", "/verify", "");
    assert!(report.contains(r#""agree":false"#), "{report}");
    assert!(report.contains(r#""digest":null"#), "{report}");
}

/// The digest notices a difference. Two copies are fed different facts behind the
/// coordinator's back - by writing to the spare directly, which is what a repair or a bug
/// looks like from the outside - and `verify` stops saying they agree.
#[test]
fn the_digest_notices_when_two_copies_differ() {
    let (a, spare, _third) = a_replicated_group();
    let fingerprint = a_replicated_group_fingerprint();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", "amount 1 5\n");
    assert!(ok(a, "GET", "/verify", "").contains(r#""agree":true"#));

    // Straight at the spare's own database, bypassing the fan-out: the spare is a primary of
    // nothing, so `/internal/import` is the only door, and this is what a write that reached
    // one copy and not the other leaves behind.
    let body = big_cluster::wire::ImportRequest {
        table: "tx".to_string(),
        keys: Vec::new(),
        facts: vec![big_cluster::wire::OwnedFact {
            field: "amount".to_string(),
            record: 99,
            value: big_cluster::wire::FactValue::Int(7),
        }],
        // Straight at the node, claiming nothing about who owns what: this is the door a
        // coordinator uses after it has already routed, and here there is no coordinator.
        routed: None,
    }
    .encode();
    let (status, _) = send_bytes(spare, "/internal/import", &body, fingerprint);
    assert_eq!(status, 200);

    let report = ok(a, "GET", "/verify", "");
    assert!(report.contains(r#""agree":false"#), "{report}");
    // Both answered; they simply do not hold the same facts.
    assert!(!report.contains(r#""digest":null"#), "{report}");
}

/// A binary POST, for the tests that have to reach an `/internal/` route the way a peer does.
///
/// Including the two stamps, because a peer sends them and a request without them is refused
/// before it is decoded. A test that skipped them would be testing the refusal.
fn send_bytes(addr: SocketAddr, target: &str, body: &[u8], fingerprint: u64) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let head = format!(
        "POST {target} HTTP/1.1\r\nHost: localhost\r\n{}: {}\r\n{}: {fingerprint:x}\r\n\
         Content-Length: {}\r\n\r\n",
        big_cluster::WIRE_HEADER,
        big_cluster::WIRE_VERSION,
        big_cluster::CLUSTER_HEADER,
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("a header block");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).expect("a status");
    (status, raw[split + 4..].to_vec())
}

// -------------------------------------------------------------------------------------------
// Giving a live range one more copy
// -------------------------------------------------------------------------------------------

/// **A range that was serving alone gains a copy, and nothing is refused while it does.**
///
/// The copy enters the group marked behind in one decision, so a promotion cannot reach it
/// before a repair has proved it agrees - and writes reach it from that moment, so it stops
/// falling further behind while it fills in.
#[test]
fn a_range_gains_a_copy_without_refusing_anything() {
    let (a, spare, third) = a_named_replicated_group("big-copy");
    // A second range, held by `a-third` alone, is what there is to give a copy to.
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/admin/cluster/split?at=64&to=a-third", "");

    let (status, body) = send(a, "POST", "/admin/cluster/replica?range=1&to=a-spare", "");
    assert_eq!(status, 200, "{body}");

    let seen = ok(a, "GET", "/cluster/topology", "");
    assert!(
        seen.contains(r#""holders":["a-third","a-spare"]"#),
        "the copy is in the group, primary unchanged: {seen}"
    );
    let _ = (spare, third);
}

/// **A learner is refused, by all three verbs that hand out a range.**
///
/// A learner receives the agreement and holds nothing - that is the state's whole purpose, so
/// that adding a node never raises the bar for an election before the node can help clear it.
/// `split` and `move` accepted one, which produced a holder the agreement does not count: a
/// range whose availability rests on a node with no vote.
#[test]
fn a_range_is_never_handed_to_a_node_that_does_not_vote() {
    let (a, _, _) = a_named_replicated_group("big-learner");
    let d = free_port();
    ok(a, "POST", &format!("/admin/cluster/node?name=d&addr={d}"), "");

    for (method, target) in [
        ("POST", "/admin/cluster/split?at=64&to=d".to_string()),
        ("POST", "/admin/cluster/replica?range=0&to=d".to_string()),
        ("POST", "/admin/cluster/move?range=0&to=d".to_string()),
    ] {
        let (status, body) = send(a, method, &target, "");
        // `refused`, the code every "the cluster will not do this" answer carries.
        assert_eq!(status, 409, "{target}: {body}");
        assert!(body.contains("admit"), "{target} names the step that fixes it: {body}");
    }
}

/// A copy can be taken away again, and the node a read goes to is not one of them.
#[test]
fn a_copy_can_be_dropped_but_the_primary_cannot() {
    let (a, _, _) = a_named_replicated_group("big-drop");

    let (status, body) = send(a, "DELETE", "/admin/cluster/replica?range=0&from=a", "");
    assert_ne!(status, 200, "the primary is not a copy: {body}");
    assert!(body.contains("cluster move"), "it names the verb that is: {body}");

    let (status, body) = send(a, "DELETE", "/admin/cluster/replica?range=0&from=a-third", "");
    assert_eq!(status, 200, "{body}");
    let seen = ok(a, "GET", "/cluster/topology", "");
    assert!(!seen.contains("a-third\"]"), "it is out of the group: {seen}");
}

// -------------------------------------------------------------------------------------------
// Joining without a file
// -------------------------------------------------------------------------------------------

/// **A node already in the cluster hands a joining one everything a file would have said.**
///
/// The whole point is that the answer is the *agreement's* membership rather than the text
/// anybody read: a node admitted after these two started is in it, and no file on any machine
/// has been edited.
#[test]
fn a_node_in_the_cluster_answers_who_is_in_it() {
    let (a, spare, third) = a_named_replicated_group("big-test");

    let (status, body) = send_bytes(
        a,
        big_cluster::path::JOIN,
        &[],
        big_cluster::config::fingerprint_of("big-test"),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));

    let (id, members) = big_cluster::wire::get_join(&body).unwrap();
    assert_eq!(id, "big-test");
    assert_eq!(
        members,
        vec![
            ("a".to_string(), a.to_string()),
            ("a-spare".to_string(), spare.to_string()),
            ("a-third".to_string(), third.to_string()),
        ],
        "every node the agreement holds, with the address a peer reaches it on"
    );

    // And what comes back builds the configuration a joining node starts from: it names every
    // peer, and claims no range for a node the cluster has not given one.
    let config = big_cluster::ClusterConfig::joining(&id, &members, "a-third").unwrap();
    assert_eq!(config.fingerprint(), big_cluster::config::fingerprint_of("big-test"));
    assert!(config.this().replica_of.is_some());
}

/// **A cluster that runs no agreement refuses the joining request, and says which step is
/// impossible rather than which one is next.**
///
/// Found by writing the end-to-end test rather than by reading: joining ends in `add-node`,
/// `add-node` is a decision, and a cluster whose ranges have no copies commits no decisions.
/// Without this the answer would name a command that is itself refused - an operator sent
/// round a loop by two messages that are each individually correct.
#[test]
fn a_cluster_that_runs_no_agreement_refuses_the_joining_request() {
    let (a, _) = two_named_nodes("big-unreplicated");

    let (status, body) = send_bytes(
        a,
        big_cluster::path::JOIN,
        &[],
        big_cluster::config::fingerprint_of("big-unreplicated"),
    );
    assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
    let said = String::from_utf8_lossy(&body);
    assert!(said.contains("no_agreement"), "{said}");
    assert!(said.contains("copy"), "it names what the cluster is missing: {said}");
}

/// **A cluster identified by the shape of its file cannot be joined, and refuses rather than
/// letting the node break later.** Handing over a membership with no name would produce a node
/// that started cleanly and had every subsequent request refused as a mismatch — the failure
/// that is hardest to read, because startup said nothing.
#[test]
fn a_cluster_with_no_name_refuses_the_joining_request() {
    let (a, _) = two_nodes();

    let (status, body) = send_bytes(a, big_cluster::path::JOIN, &[], shape_fingerprint());
    assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
    let said = String::from_utf8_lossy(&body);
    assert!(said.contains("cluster_unnamed"), "{said}");
    assert!(said.contains("cluster_id"), "it names the key to set: {said}");
}

// -------------------------------------------------------------------------------------------
// Failing over on its own
// -------------------------------------------------------------------------------------------

/// The clocks a test can afford to wait for.
///
/// A datacentre's defaults are seconds, because an election held over a garbage collector pause
/// costs more than a second of waiting. A test cannot spend those seconds, so it says so out
/// loud: the *rules* are what these tests are about, and they do not depend on the numbers.
fn brisk() -> (Timing, Leases) {
    (
        Timing { election_min: 150, election_spread: 150, heartbeat: 50 },
        Leases {
            serve_for: std::time::Duration::from_millis(300),
            promote_after: std::time::Duration::from_millis(900),
            ..Leases::default()
        },
    )
}

/// One range, three copies, all of them running the agreement.
fn a_group_of_three() -> (String, Vec<SocketAddr>) {
    let ports: Vec<SocketAddr> = (0..3).map(|_| free_port()).collect();
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{}\"\nshards = \"0..\"\n\
         [[node]]\nname = \"b\"\naddr = \"{}\"\nreplica = \"a\"\n\
         [[node]]\nname = \"c\"\naddr = \"{}\"\nreplica = \"a\"\n",
        ports[0], ports[1], ports[2]
    );
    (file, ports)
}

/// The same as `brisk`, and the agreement is also allowed to move the row-key namespace.
///
/// Fifteen seconds in a deployment, a quarter of a second here, and for the same reason the
/// other clocks are shortened: what these tests are about is the rule, and the rule does not
/// depend on the number. The ordering does, and it is kept - the namespace still waits longer
/// than a range does.
fn brisk_electing() -> (Timing, Leases) {
    let (timing, leases) = brisk();
    (timing, Leases { move_schema_after: Some(leases.promote_after * 3), ..leases })
}

/// A node a test can take away.
///
/// `close` drops the listener rather than only stopping the loop: a port that accepts and never
/// answers is not a dead node, it is a slow one, and the two fail differently.
struct Node {
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    done: Option<std::thread::JoinHandle<()>>,
}

impl Node {
    fn close(&mut self) {
        self.running.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.done.take() {
            let _ = h.join();
        }
    }
}

impl Node {
    /// Leaves the node running for the life of the test process.
    ///
    /// For the nodes a test needs up and never takes away: holding the handle would mean
    /// naming it, and a name that is only there to avoid a drop is a name that reads as a
    /// mistake.
    fn forget(mut self) {
        self.done = None;
        std::mem::forget(self);
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.close();
    }
}

/// Starts one node of an agreeing group, on the brisk clocks above.
fn start_agreeing(file: &str, name: &str, addr: SocketAddr) -> Node {
    let (timing, leases) = brisk();
    start_with(file, name, addr, timing, leases, None)
}

/// The same, with the clocks and the state file spelled out.
///
/// `state` is where the agreement keeps the three things a restart may not lose. `None` is
/// [`Forgetful`], which is right for a node no test restarts and wrong for one that does - a
/// node that forgets its vote can cast a second one in the same term.
fn start_with(
    file: &str,
    name: &str,
    addr: SocketAddr,
    timing: Timing,
    leases: Leases,
    state: Option<std::path::PathBuf>,
) -> Node {
    start_configured(file, name, addr, timing, leases, state, ServerConfig::default())
}

/// On the brisk clocks, with the balancer switched on - the one thing the steward acts on.
fn start_balancing(file: &str, name: &str, addr: SocketAddr) -> Node {
    let (timing, leases) = brisk();
    let config = ServerConfig {
        balance: big_cluster::balance::Policy {
            enabled: true,
            ..big_cluster::balance::Policy::default()
        },
        ..ServerConfig::default()
    };
    start_configured(file, name, addr, timing, leases, None, config)
}

/// The same, with the server's configuration spelled out as well.
fn start_configured(
    file: &str,
    name: &str,
    addr: SocketAddr,
    timing: Timing,
    leases: Leases,
    state: Option<std::path::PathBuf>,
    server: ServerConfig,
) -> Node {
    let config = ClusterFile::parse(file).unwrap().for_node(Some(name), "").unwrap();
    let store: Box<dyn big_cluster::raft::Store> = match state {
        Some(path) => Box::new(big_cluster::raft::FileStore::new(path)),
        None => Box::new(Forgetful),
    };
    let cluster =
        Cluster::with_timing(Api::in_memory().unwrap(), config, None, store, timing, leases)
            .unwrap();
    let server = Server::bind_cluster(cluster, addr, server).expect("the port was free");
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = std::sync::Arc::clone(&running);
    let done = std::thread::spawn(move || {
        let _ = server.serve_while(&flag);
        // `server` is dropped here, which is what closes the port.
    });
    Node { running, done: Some(done) }
}

/// Waits for a condition, or gives up and says what it saw last.
fn until(what: &str, check: impl FnMut() -> bool) {
    waiting(what, std::time::Duration::from_secs(15), check)
}

/// The same, for a test that is waiting on the clocks a deployment actually runs.
fn waiting(what: &str, budget: std::time::Duration, mut check: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while started.elapsed() < budget {
        if check() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("gave up waiting for {what} after {:?}", started.elapsed());
}

fn ready(addr: SocketAddr) -> String {
    send(addr, "GET", "/ready", "").1
}

/// **The whole point.** The node that was serving a range dies, and the range keeps being
/// served - by a node that already held every record, decided by an agreement rather than by a
/// person editing a file.
#[test]
fn a_range_fails_over_when_its_primary_dies() {
    let (file, ports) = a_group_of_three();
    let mut a = start_agreeing(&file, "a", ports[0]);
    let _b = start_agreeing(&file, "b", ports[1]);
    let _c = start_agreeing(&file, "c", ports[2]);

    // The agreement settles before anything is asked of it.
    until("an elected leader", || ready(ports[1]).contains(r#""leader":""#));

    ok(ports[0], "POST", "/table/tx", "");
    ok(ports[0], "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(ports[0], "POST", "/table/tx/import", "amount 1 5\namount 2 5\n");
    assert_eq!(ok(ports[1], "POST", "/table/tx/query", "Count(All())"), r#"{"count":2}"#);

    // `a` goes away. Nothing else is touched: no config is edited and nobody is told.
    a.close();

    // `b` and `c` are a majority. One of them notices `a` has stopped answering, and the
    // agreement moves the range to whichever of them is still there.
    until("the range to move", || {
        let (status, body) = send(ports[1], "POST", "/table/tx/query", "Count(All())");
        status == 200 && body == r#"{"count":2}"#
    });

    // Every node that is left agrees who serves it now. A coordinator learns on the next
    // heartbeat, so this is a wait rather than an assertion: the map is agreed at once and
    // arrives in its own time.
    until("both survivors to route to the new primary", || {
        [ports[1], ports[2]]
            .iter()
            .all(|p| send(*p, "POST", "/table/tx/query", "Count(All())").1 == r#"{"count":2}"#)
    });
}

/// **A range still fails over after the leader has compacted its log.**
///
/// The guard in front of a promotion compared the commit index with the length of the vector
/// holding the log's suffix, and the two could never agree again once anything had been
/// compacted away - so every promotion after the first compaction was refused, silently, on a
/// cluster reporting itself healthy. The rule is proven against a simulated agreement in
/// `big-cluster/tests/raft.rs`; this is the one test that runs it inside the real driver
/// thread, against a log the real leader compacted on its own.
#[test]
fn a_range_fails_over_after_the_log_has_been_compacted() {
    let (file, ports) = a_group_of_three();
    // **The copies elect a leader before the primary arrives.** Only the leader compacts its
    // log - a follower's base moves only when it is handed a snapshot - so the node that dies
    // must not be the one that decides, or its successor would decide from an uncompacted log
    // and this test would pass with the bug in place. `a` is the primary by the file; the
    // agreement's leader has to be one of the other two.
    let _b = start_agreeing(&file, "b", ports[1]);
    let _c = start_agreeing(&file, "c", ports[2]);
    until("a leader among the copies", || ready(ports[1]).contains(r#""leader":""#));
    let mut a = start_agreeing(&file, "a", ports[0]);
    until("the primary to join", || ready(ports[0]).contains(r#""leader":""#));
    let leader = leader_of(&ports);
    assert_ne!(leader, ports[0], "the primary joined a settled agreement; it must not lead it");

    ok(ports[0], "POST", "/table/tx", "");
    ok(ports[0], "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(ports[0], "POST", "/table/tx/import", "amount 1 5\namount 2 5\n");

    // Enough committed decisions that the leader compacts. Every split and every merge is one
    // entry, and with every voter up the leader drops whatever is more than `KEEP_ENTRIES`
    // behind - so more than that many, with room, is a log that has certainly been cut.
    for _ in 0..(big_cluster::raft::Raft::KEEP_ENTRIES / 2 + 8) {
        ok(leader, "POST", "/admin/cluster/split?at=900", "");
        ok(leader, "POST", "/admin/cluster/merge?range=0", "");
    }

    a.close();
    until("the range to move after a compaction", || {
        let (status, body) = send(ports[1], "POST", "/table/tx/query", "Count(All())");
        status == 200 && body == r#"{"count":2}"#
    });
}

/// **A node cut off from the agreement stops answering for its range.**
///
/// This is the half that makes the other half safe. A failure detector alone cannot tell a
/// dead node from an unreachable one, so the node that might have been replaced has to be the
/// one that stops - and it stops on its own clock, without being told.
#[test]
fn a_node_that_loses_the_agreement_stops_serving() {
    let (file, ports) = a_group_of_three();
    let a = start_agreeing(&file, "a", ports[0]);
    let mut b = start_agreeing(&file, "b", ports[1]);
    let mut c = start_agreeing(&file, "c", ports[2]);

    until("an elected leader", || ready(ports[0]).contains(r#""leader":""#));
    ok(ports[0], "POST", "/table/tx", "");
    ok(ports[0], "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");

    // Everybody except `a` stops. `a` is alone: it cannot be a majority, so it cannot know
    // whether the other two have given its range to somebody else.
    b.close();
    c.close();

    until("`a` to stand down", || !ready(ports[0]).contains(r#""serving":true"#));

    // And it says so rather than answering from a copy it can no longer vouch for.
    let (status, body) = send(ports[0], "POST", "/table/tx/query", "Count(All())");
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(r#""code":"not_serving""#), "{body}");
    let _ = a;
}

/// A range with no copy is not fenced. There is nothing to fail over to, so a node that stops
/// hearing from anybody has not lost anything - and stopping would be an outage invented
/// rather than avoided.
#[test]
fn a_range_with_no_copy_keeps_serving_alone() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", "amount 1 5\n");

    // No agreement is running at all, so there is no lease to lose.
    let body = ok(a, "GET", "/ready", "");
    assert!(!body.contains(r#""term""#), "{body}");
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":1}"#);
    let _ = b;
}

/// **A copy that is behind is not promoted, even when it is the only one left.**
///
/// This is the load-bearing half of letting a write stand when a spare is unreachable. Without
/// it, the copy that missed the write is exactly the copy that would answer after the next
/// failure - which would turn a write everybody was told succeeded into a read nobody can tell
/// is wrong.
///
/// The copy is brought back *empty*, which is the worst case and also the realistic one: a
/// machine that was replaced.
#[test]
fn a_copy_that_is_behind_is_never_promoted() {
    let (a, spare, other) = (free_port(), free_port(), free_port());
    // `other` holds a range of its own, so a majority survives losing both nodes of the first
    // range - the agreement stays able to decide, and what it decides is nothing.
    let file = format!(
        "schema_leader = \"other\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..1\"\n\
         [[node]]\nname = \"other\"\naddr = \"{other}\"\nshards = \"1..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let mut a_node = start_agreeing(&file, "a", a);
    let mut spare_node = start_agreeing(&file, "a-spare", spare);
    let _other = start_agreeing(&file, "other", other);

    until("an elected leader", || ready(other).contains(r#""leader":""#));
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");

    // The spare goes away, and a write lands without it. The write stands and says so.
    spare_node.close();
    until("the spare to be marked behind", || ready(other).contains(r#""a-spare""#));
    let (status, body) = send(a, "POST", "/table/tx/import", "amount 1 5\n");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("a-spare"), "{body}");

    // It comes back with nothing in it, which is what a replaced machine looks like.
    let _spare_again = start_agreeing(&file, "a-spare", spare);
    until("the spare to be answering again", || send(spare, "GET", "/ready", "").0 == 200);

    // Now the node that was serving the range dies. The only other copy of it is the one that
    // missed the write, so the range does not move - and says so rather than answering from a
    // database that is missing a record.
    a_node.close();
    until("the range to be reported unreachable", || {
        send(other, "POST", "/table/tx/query", "Count(All())").0 == 503
    });
    for _ in 0..8 {
        let (status, body) = send(other, "POST", "/table/tx/query", "Count(All())");
        assert_eq!(status, 503, "the range was given to a copy that had missed a write: {body}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // The other range is untouched by any of it.
    assert_eq!(
        send(other, "GET", "/ready", "").0,
        200,
        "a range with its own primary was taken down by somebody else's failure"
    );
}

/// **A repair puts a copy back into service.**
///
/// The other half of letting a write stand when a spare is unreachable. Without it one blip
/// costs a cluster its redundancy for good, because a copy marked behind is a copy that will
/// never be promoted - so the mark has to be clearable, and clearing it has to mean something.
#[test]
fn a_repair_catches_a_copy_up_and_lets_it_serve_again() {
    let (a, spare, other) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"other\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..1\"\n\
         [[node]]\nname = \"other\"\naddr = \"{other}\"\nshards = \"1..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let mut a_node = start_agreeing(&file, "a", a);
    let mut spare_node = start_agreeing(&file, "a-spare", spare);
    let _other = start_agreeing(&file, "other", other);

    until("an elected leader", || ready(other).contains(r#""leader":""#));
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");
    ok(a, "POST", "/table/tx/import", "amount 1 5\ncountry 1 GB\n");

    // The spare goes away and misses a write, including a row key it has never heard of.
    spare_node.close();
    until("the spare to be marked behind", || ready(other).contains(r#""a-spare""#));
    let (status, body) = send(a, "POST", "/table/tx/import", "amount 2 9\ncountry 2 FR\n");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("a-spare"), "{body}");

    // It comes back empty, and the copies do not agree.
    let _spare_again = start_agreeing(&file, "a-spare", spare);
    until("the spare to be answering again", || send(spare, "GET", "/ready", "").0 == 200);
    let report = ok(a, "GET", "/verify", "");
    assert!(report.contains(r#""agree":false"#), "{report}");

    // The repair copies what differs, and only what differs.
    let repaired = ok(a, "POST", "/repair", "");
    assert!(repaired.contains(r#""node":"a-spare""#), "{repaired}");
    assert!(repaired.contains(r#""outcome":"caught up""#), "{repaired}");

    // Now they agree, digest for digest - which includes the row keys, so the copy answers
    // `GroupBy` with names rather than nulls.
    until("the copies to agree", || ok(a, "GET", "/verify", "").contains(r#""agree":true"#));
    until("the mark to be cleared", || ready(other).contains(r#""behind":[]"#));

    // And the proof that it means something: the range now fails over to the copy that was
    // behind, holding every record including the one it had missed.
    a_node.close();
    until("the range to move to the repaired copy", || {
        send(other, "POST", "/table/tx/query", "Count(All())").1 == r#"{"count":2}"#
    });
    assert_eq!(
        ok(other, "POST", "/table/tx/query", r#"Count(Row(country="FR"))"#),
        r#"{"count":1}"#,
        "the repaired copy is missing the row key it was told about"
    );
}

/// Two batches with different keys, through a coordinator that is not the schema leader.
///
/// The shape that matters: the node planning the write is not the node handing out row ids, so
/// every key it has not seen costs a round trip and comes back as a number it has to accept.
/// A second batch must not be able to collide with the first.
#[test]
fn a_second_batch_of_keys_does_not_collide_with_the_first() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"b\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..1\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"1..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let mut a_spare = start_agreeing(&file, "a-spare", spare);
    start_agreeing(&file, "a", a).forget();
    start_agreeing(&file, "b", b).forget();
    // `a` holds a replicated range, so it is fenced until it has heard from the agreement.
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");
    ok(a, "POST", "/table/tx/import", "amount 1 100\ncountry 1 GB\n");

    // The copy goes away between the batches, which is the shape that found this.
    a_spare.close();
    let (status, body) = send(a, "POST", "/table/tx/import", "amount 2 900\ncountry 2 FR\n");
    assert_eq!(status, 200, "{body}");

    // And both keys mean what they were told they mean, on the node that holds them.
    let groups = ok(a, "POST", "/table/tx/query", r#"Distinct(All(), field="country")"#);
    assert_eq!(
        groups,
        r#"{"groups":[{"key":"FR","row":1,"value":{"count":1}},{"key":"GB","row":0,"value":{"count":1}}]}"#,
        "{groups}"
    );
}

/// `/metrics` says what an operator needs to alert on, and `copies_behind` is the one that
/// matters: it is redundancy this cluster has lost and will not get back on its own.
#[test]
fn metrics_report_the_cluster() {
    let (a, b) = two_nodes();
    let text = ok(a, "GET", "/metrics", "");
    assert!(text.contains("big_cluster_nodes 2"), "{text}");
    assert!(text.contains("big_cluster_copies_behind 0"), "{text}");
    // A range with no copy is never fenced, so this node is serving whatever happens elsewhere.
    assert!(text.contains("big_cluster_serving 1"), "{text}");

    // A fan-out is visible as peer traffic, and a peer that is not there is visible as
    // something else. Both are counted, because an operator's next move differs.
    ok(a, "POST", "/table/tx", "");
    let before = text.contains("big_cluster_peer_requests_total 0");
    let after = ok(a, "GET", "/metrics", "");
    assert!(before, "{text}");
    assert!(!after.contains("big_cluster_peer_requests_total 0"), "{after}");
    let _ = b;
}

/// **A peer running a different build is refused before anything is decoded.**
///
/// Two builds that disagree about the encoding read each other's messages as something else - a
/// length where a tag was - and the result is not a refusal, it is an answer that is quietly
/// wrong. That is the one failure this whole layer exists to avoid, so it costs a header.
#[test]
fn a_peer_speaking_a_different_wire_version_is_refused() {
    let (a, _b) = two_nodes();
    let body = big_cluster::wire::FragmentsRequest { table: "tx".to_string() }.encode();
    let fingerprint = LAST_FILE.with(|f| {
        ClusterFile::parse(&f.borrow()).unwrap().for_node(Some("a"), "").unwrap().fingerprint()
    });

    // The right version: refused for a different reason, which is that there is no such table.
    let (status, _) = send_bytes(a, "/internal/fragments", &body, fingerprint);
    assert_ne!(status, 409, "a peer that agrees should get past the check");

    // No stamps at all, which is what an older build sends.
    let mut stream = TcpStream::connect(a).unwrap();
    let head = format!(
        "POST /internal/fragments HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&body).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 409"), "{raw}");
    assert!(raw.contains(r#""code":"wire_version""#), "{raw}");
}

/// **Two nodes reading cluster files that disagree cannot serve each other.**
///
/// This is the failure ownership-by-configuration could not see: a range moved on one machine
/// and not the other, each answering part of every query, neither saying so. They cannot meet
/// without exchanging a fingerprint, so now they meet and stop.
#[test]
fn a_peer_reading_a_different_cluster_file_is_refused() {
    let (a, _b) = two_nodes();
    let body = big_cluster::wire::FragmentsRequest { table: "tx".to_string() }.encode();

    let (status, out) = send_bytes(a, "/internal/fragments", &body, 0xdead_beef);
    assert_eq!(status, 409, "{}", String::from_utf8_lossy(&out));
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(r#""code":"cluster_mismatch""#), "{text}");
    // The message names what this node expected, because whoever reads it is looking at two
    // machines and has to know which one to correct.
    assert!(text.contains("deadbeef"), "{text}");
}

// -------------------------------------------------------------------------------------------
// The clocks and the state file a deployment actually uses
// -------------------------------------------------------------------------------------------

/// **The same failover, on the clocks that ship**, over real sockets.
///
/// Every other test here runs the agreement fast so that it can watch it. This one runs it at
/// the speed a datacentre does - an election of one and a half to three seconds, a promotion
/// after four and a half - because numbers that are only ever exercised by hand are numbers
/// that drift. It is the slowest test in the tree and it earns it.
#[test]
fn a_range_fails_over_on_the_clocks_it_ships_with() {
    let (file, ports) = a_group_of_three();
    let mut a = start_with(&file, "a", ports[0], Timing::default(), Leases::default(), None);
    let _b = start_with(&file, "b", ports[1], Timing::default(), Leases::default(), None);
    let _c = start_with(&file, "c", ports[2], Timing::default(), Leases::default(), None);

    // An election is at most `election_min + election_spread`, twice over if the first splits.
    waiting("an elected leader", std::time::Duration::from_secs(15), || {
        ready(ports[1]).contains(r#""leader":""#)
    });

    ok(ports[0], "POST", "/table/tx", "");
    ok(ports[0], "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(ports[0], "POST", "/table/tx/import", "amount 1 5\namount 2 5\n");

    a.close();

    // `promote_after` plus an election, plus room for a loaded machine.
    waiting("the range to move", std::time::Duration::from_secs(30), || {
        send(ports[1], "POST", "/table/tx/query", "Count(All())").1 == r#"{"count":2}"#
    });

    let (status, body) = send(ports[1], "POST", "/table/tx/import", "amount 3 5\n");
    assert_eq!(status, 200, "{body}");
}

/// **A node that restarts remembers what it voted.**
///
/// A vote forgotten in a restart is a vote that can be cast twice, which is two leaders in one
/// term. The encoding of that state is unit-tested; this is the other half - that a running
/// node writes it and reads it back, which nothing else here exercises because every other test
/// uses `Forgetful`.
///
/// The node taken away is a *copy*, not the one serving the range: this test is about the
/// agreement's state surviving, and restarting the serving node would drag in a different
/// question - see `a_serving_node_that_comes_back_empty_is_not_repaired`.
#[test]
fn a_node_that_restarts_keeps_what_it_agreed() {
    let dir = tempfile::tempdir().unwrap();
    let (file, ports) = a_group_of_three();
    let state = |name: &str| Some(dir.path().join(format!("{name}.raft")));
    let (timing, leases) = brisk();

    let _a = start_with(&file, "a", ports[0], timing, leases, state("a"));
    let _b = start_with(&file, "b", ports[1], timing, leases, state("b"));
    let mut c = start_with(&file, "c", ports[2], timing, leases, state("c"));

    until("an elected leader", || ready(ports[0]).contains(r#""leader":""#));
    ok(ports[0], "POST", "/table/tx", "");
    ok(ports[0], "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(ports[0], "POST", "/table/tx/import", "amount 1 5\n");

    let term_before = term_of(ready(ports[2]));
    assert!(term_before > 0, "nothing was agreed before the restart");
    c.close();

    // **The mark is earned while it is away, not when it comes back.** Nothing inspects a
    // returning node's records - the only thing that marks a copy behind is the leader not
    // hearing from it for `promote_after` - so the test waits for that to have happened
    // rather than assuming a restart takes longer than the lease. Without this wait the
    // node is back inside the window, is never marked, and the assertion below is left
    // waiting on something no mechanism will ever do.
    until("the copy that went away to be marked behind", || {
        ready(ports[0]).contains(r#""behind":["c"]"#)
    });

    // The same node, the same file. It has to come back knowing what it knew: a node that
    // restarts at term zero has forgotten a vote it may already have cast.
    let _c_again = start_with(&file, "c", ports[2], timing, leases, state("c"));
    until("the restarted node to answer", || send(ports[2], "GET", "/ready", "").0 == 200);
    until("it to rejoin the agreement", || {
        term_of(ready(ports[2])) >= term_before && ready(ports[2]).contains(r#""leader":""#)
    });

    // The file was read rather than started fresh: the state on disk says so.
    let written = std::fs::read(dir.path().join("c.raft")).unwrap();
    assert!(written.starts_with(b"BIGRAFT3"), "the agreement wrote no state for `c`");

    // Reads never stopped: the copy serving the range was never the one taken away.
    assert_eq!(ok(ports[0], "POST", "/table/tx/query", "Count(All())"), r#"{"count":1}"#);

    // **Its database did not come back with it.** The agreement state persisted and the
    // records did not, which is what a replaced machine looks like - and it goes through the
    // same two mechanisms as any other copy that is behind: it is marked, so it cannot be
    // promoted, and a repair is what clears that. Coming back does not clear the mark:
    // rejoining proves the node is answering again, not that it holds what it missed.
    assert!(
        ready(ports[0]).contains(r#""behind":["c"]"#),
        "rejoining cleared the mark; only a repair may"
    );
    assert!(ok(ports[0], "GET", "/verify", "").contains(r#""agree":false"#));

    let repaired = ok(ports[0], "POST", "/repair", "");
    assert!(repaired.contains(r#""outcome":"caught up""#), "{repaired}");
    until("the copies to agree again", || {
        ok(ports[0], "GET", "/verify", "").contains(r#""agree":true"#)
    });
}

/// **The copy serving a range is the truth, even when its disk was replaced.**
///
/// This is what choosing consistency costs, pinned rather than discovered. Every write reaches
/// the copy serving a range before any other, so nothing in the cluster is in a position to
/// contradict it - and a node that comes back with an empty disk fast enough that nothing was
/// promoted goes on serving a range it no longer holds.
///
/// **Nothing automatic notices.** It was not gone long enough to be marked behind, so there is
/// no flag and no failover; there is nothing to fail over *to*, because as far as the
/// agreement can tell the right node is answering. `GET /verify` is the only thing that finds
/// it, which is exactly why that route exists and why the runbook says to run it - and
/// `POST /repair` says plainly that this is the one it cannot fix.
#[test]
fn a_serving_node_that_comes_back_empty_is_only_found_by_verify() {
    let (file, ports) = a_group_of_three();
    // Elections stay brisk so the test is quick; the *promotion* window is long, so that a
    // node restarting inside it is not replaced - which is the case being pinned.
    let timing = Timing { election_min: 150, election_spread: 150, heartbeat: 50 };
    let leases = Leases {
        serve_for: std::time::Duration::from_secs(5),
        promote_after: std::time::Duration::from_secs(30),
        ..Leases::default()
    };
    let mut a = start_with(&file, "a", ports[0], timing, leases, None);
    let _b = start_with(&file, "b", ports[1], timing, leases, None);
    let _c = start_with(&file, "c", ports[2], timing, leases, None);

    until("an elected leader", || ready(ports[1]).contains(r#""leader":""#));
    ok(ports[0], "POST", "/table/tx", "");
    ok(ports[0], "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(ports[0], "POST", "/table/tx/import", "amount 1 5\n");
    assert_eq!(ok(ports[1], "POST", "/table/tx/query", "Count(All())"), r#"{"count":1}"#);

    // Away and back well inside the promotion window, with nothing on its disk.
    a.close();
    let _a_again = start_with(&file, "a", ports[0], timing, leases, None);
    until("the replaced node to answer", || send(ports[0], "GET", "/ready", "").0 == 200);

    // **It refuses rather than answers**, and that is worth more than it looks: the schema
    // went with the data, so the first thing any client sees is a node saying it does not
    // have the table - naming itself. The silent version of this failure would be a node that
    // kept its schema and lost its records, answering a smaller number than the truth.
    until("the query to fail on the emptied node", || {
        send(ports[1], "POST", "/table/tx/query", "Count(All())").0 == 404
    });
    let (status, body) = send(ports[1], "POST", "/table/tx/query", "Count(All())");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains(r#""code":"peer_refused""#), "{body}");
    assert!(body.contains("`a`"), "the refusal does not name the node that lost its data: {body}");

    // And nothing *flagged* it. It was never unreachable for long enough to be marked, and no
    // other copy was ever a better answer, so the agreement had nothing to decide. The loud
    // failure above is the engine refusing a question it cannot answer, not the cluster
    // noticing a node is behind.
    assert!(ready(ports[1]).contains(r#""behind":[]"#), "{}", ready(ports[1]));

    // `verify` is the only thing that finds it, and `repair` says which copy it cannot take
    // from rather than claiming to have fixed something.
    let report = ok(ports[1], "GET", "/verify", "");
    assert!(report.contains(r#""agree":false"#), "{report}");
    let repaired = ok(ports[1], "POST", "/repair", "");
    assert!(
        repaired.contains("this copy is the one serving its range")
            || repaired == r#"{"repaired":[]}"#,
        "the repair claimed to have fixed something it cannot: {repaired}"
    );
}

/// The agreement's term out of a `/ready` body.
fn term_of(ready: String) -> u64 {
    ready
        .split(r#""term":"#)
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no term in {ready}"))
}

/// The bound a `TopN` is asked for widens until the answer is provably the whole cluster's.
///
/// **This is the case the bound exists to get right, and the one it could get wrong.** Each node
/// is asked for more than `n` groups but not for all of them, so a group that is below *both*
/// nodes' cut is invisible in the first round - and here that group is the winner. `A` through
/// `E` hold ten each and live only on `a`; `F` through `J` hold ten each and live only on `b`;
/// `X` holds nine on each node, which is eighteen and beats all of them, and is sixth on both.
///
/// A bound that stopped at the first round would answer `A` with ten. The answer is `X` with
/// eighteen, because the first round's thresholds say each node is still holding groups worth up
/// to ten - which is not less than the ten a candidate leads with, so nothing is certain yet and
/// the bound widens. The second round asks for more than either node has, and every threshold
/// becomes zero.
#[test]
fn a_top_n_widens_its_bound_until_a_group_hidden_on_every_node_can_be_seen() {
    let (a, b) = two_nodes();
    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");

    let mut facts = String::new();
    let mut id = 1;
    let put = |facts: &mut String, id: &mut u64, key: &str, base: u64, times: usize| {
        for _ in 0..times {
            facts.push_str(&format!("country {} {key}\n", base + *id));
            *id += 1;
        }
    };
    // Ten each on `a` alone, and ten each on `b` alone: five leaders per node, none shared.
    for key in ["A", "B", "C", "D", "E"] {
        put(&mut facts, &mut id, key, 0, 10);
    }
    for key in ["F", "G", "H", "I", "J"] {
        put(&mut facts, &mut id, key, WIDTH, 10);
    }
    // Nine on each node, so sixth on both and first overall.
    put(&mut facts, &mut id, "X", 0, 9);
    put(&mut facts, &mut id, "X", WIDTH, 9);
    ok(a, "POST", "/table/tx/import", &facts);

    let top = ok(b, "POST", "/table/tx/query", r#"TopN(All(), field="country", n=1)"#);
    assert!(top.contains(r#""key":"X""#), "{top}");
    assert!(top.contains(r#""count":18"#), "{top}");

    // And the ranking below it, which a bound that stopped early would also have cut wrong.
    let three = ok(a, "POST", "/table/tx/query", r#"TopN(All(), field="country", n=3)"#);
    let counts: Vec<&str> =
        three.match_indices("\"count\":").map(|(i, _)| &three[i + 8..]).collect();
    assert!(counts.first().is_some_and(|c| c.starts_with("18")), "{three}");
    // The other two are tens, whichever pair of the ten-count groups the tie-break picks.
    assert_eq!(three.matches("\"count\":10").count(), 2, "{three}");
}

// -------------------------------------------------------------------------------------------
// Reshaping the cluster while it runs
//
// The map used to be whatever `cluster.toml` said, for the life of every process that read it.
// It is a value the agreement decides now, and these are the two things an operator can do to
// it without stopping anybody.
// -------------------------------------------------------------------------------------------

/// **Scale-out that moves no bytes.** Cutting the open tail above everything written so far and
/// handing the upper half to another node costs one entry in the agreement and nothing on the
/// wire - and reads and writes never stop.
#[test]
fn a_tail_split_hands_a_range_over_without_copying_anything() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    // One record low in the space and one high, so both existing ranges hold something.
    ok(a, "POST", "/table/tx/import", "amount 1 5\n");
    ok(a, "POST", "/table/tx/import", &format!("amount {} 9\n", 70 * (1 << 20)));
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":2}"#);

    let before = ok(a, "GET", "/cluster/topology", "");
    assert!(before.contains(r#""shards":"0..64""#), "{before}");
    assert!(before.contains(r#""shards":"64..""#), "{before}");

    // Split the tail well above anything written, and give the empty half to `a`. `a` now
    // serves two ranges, which the map could not even express before.
    let split = leader_of(&[a, b, spare]);
    let body = ok(split, "POST", "/admin/cluster/split?at=900&to=a", "");
    assert!(body.contains(r#""range":2"#), "{body}");

    until("every node to see the new range", || {
        [a, b, spare].iter().all(|p| ok(*p, "GET", "/cluster/topology", "").contains(r#""900..""#))
    });

    // **The count is still two.** `a` holds two ranges now, and a fan-out that asked it twice
    // without saying which one would answer three.
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":2}"#);
    assert_eq!(ok(b, "POST", "/table/tx/query", "Count(All())"), r#"{"count":2}"#);

    // And the new range takes writes, on the node it was handed to.
    let high = 1_000 * (1 << 20);
    ok(a, "POST", "/table/tx/import", &format!("amount {high} 11\n"));
    assert_eq!(ok(b, "POST", "/table/tx/query", "Count(All())"), r#"{"count":3}"#);
    assert_eq!(
        ok(b, "POST", "/table/tx/query", "Sum(All(), field=\"amount\")"),
        r#"{"sum":25}"#,
        "every owner contributed exactly once"
    );
}

/// A range with records in it cannot change hands without a copy, so it is refused rather than
/// silently losing them.
#[test]
fn splitting_a_populated_half_onto_another_node_is_refused() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", &format!("amount {} 9\n", 1_000 * (1 << 20)));

    let leader = leader_of(&[a, b, spare]);
    let (status, body) = send(leader, "POST", "/admin/cluster/split?at=900&to=a", "");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("cannot change hands without a copy"), "{body}");

    // Nothing changed: the map still has two ranges.
    let after = ok(a, "GET", "/cluster/topology", "");
    assert!(!after.contains(r#""900..""#), "{after}");
}

/// Splitting and merging back is the map it started from, so a cut made too eagerly is
/// recoverable rather than permanent.
#[test]
fn a_split_can_be_merged_back() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    let leader = leader_of(&[a, b, spare]);
    ok(leader, "POST", "/admin/cluster/split?at=900", "");
    until("the split to land", || {
        ok(leader, "GET", "/cluster/topology", "").contains(r#""900..""#)
    });

    // The two halves are still both `b`'s, which is what makes them mergeable.
    ok(leader, "POST", "/admin/cluster/merge?range=1", "");
    until("the merge to land", || {
        !ok(leader, "GET", "/cluster/topology", "").contains(r#""900..""#)
    });
    let after = ok(leader, "GET", "/cluster/topology", "");
    assert!(after.contains(r#""shards":"64..""#), "{after}");
}

/// Whichever node an operator reaches, only one decides - so a command sent to a follower says
/// where to send it rather than doing half of it.
#[test]
fn a_reshape_asked_of_a_follower_names_the_node_that_decides() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    let leader = leader_of(&[a, b, spare]);
    let follower = *[a, b, spare].iter().find(|p| **p != leader).expect("three nodes");
    let (status, body) = send(follower, "POST", "/admin/cluster/split?at=900", "");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("does not decide the map"), "{body}");
}

/// The node that leads the agreement, asked of whichever of these is answering.
fn leader_of(ports: &[SocketAddr]) -> SocketAddr {
    let body = ready(ports[0]);
    let name = body
        .split(r#""leader":""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a leader")
        .to_string();
    let index = ["a", "b", "a-spare"].iter().position(|n| *n == name).expect("a known node");
    ports[index]
}

// -------------------------------------------------------------------------------------------
// Who is in the cluster, changed while it runs
// -------------------------------------------------------------------------------------------

/// **A node joins as a learner and holds nothing.** It replicates the log without voting,
/// because a node still catching up cannot help elect anybody and counting it would raise the
/// bar for every election.
#[test]
fn a_node_joins_as_a_learner_and_is_promoted_when_it_has_something_to_serve() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let d = free_port();
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    let leader = leader_of(&[a, b, spare]);
    let body = ok(leader, "POST", &format!("/admin/cluster/node?name=d&addr={d}"), "");
    assert!(body.contains(r#""node":"d""#), "{body}");

    until("every node to know about it", || {
        [a, b, spare].iter().all(|p| ok(*p, "GET", "/cluster/topology", "").contains(r#""d""#))
    });
    let seen = ok(a, "GET", "/cluster/topology", "");
    assert!(seen.contains(r#""name":"d","addr""#), "{seen}");
    assert!(seen.contains(r#""state":"learner""#), "a joining node does not vote yet: {seen}");

    // It holds no range, so nothing reads from it and nothing is at risk while it catches up.
    assert!(!seen.contains(r#""primary":"d""#), "{seen}");

    // Promoted deliberately, which is what lets it take a range.
    ok(leader, "POST", "/admin/cluster/admit?name=d", "");
    until("it to become a full member", || {
        ok(a, "GET", "/cluster/topology", "").contains(r#""name":"d","addr""#)
            && !ok(a, "GET", "/cluster/topology", "").contains(r#""state":"learner""#)
    });
}

/// **A node that joined is admitted by the steward**, once it is answering for data and holds
/// the log - and by nothing else. The test above promotes by hand; this one starts the fourth
/// node for real, with a file that names the whole cluster and itself, and waits for the
/// agreement's leader to decide it counts.
#[test]
fn a_learner_that_has_caught_up_is_admitted_by_the_steward() {
    let (a, b, spare, d) = (free_port(), free_port(), free_port(), free_port());
    // Named, so that a node whose file describes four nodes is recognised by three whose files
    // describe three: without a `cluster_id` the shape of the file is what nodes check.
    let file = format!("cluster_id = \"grows\"\n{}", three(a, b, spare));
    let _a = start_balancing(&file, "a", a);
    let _b = start_balancing(&file, "b", b);
    let _spare = start_balancing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    let leader = leader_of(&[a, b, spare]);
    ok(leader, "POST", &format!("/admin/cluster/node?name=d&addr={d}"), "");

    // The node itself, seeded as a copy of `a` - the shape that parses without overlapping
    // anybody's range, and one the agreement overwrites the moment it is heard from.
    let grown = format!("{file}[[node]]\nname = \"d\"\naddr = \"{d}\"\nreplica = \"a\"\n");
    let _d = start_balancing(&grown, "d", d);

    until("the steward to admit it", || {
        let seen = ok(a, "GET", "/cluster/topology", "");
        seen.contains(r#""name":"d","addr""#) && !seen.contains(r#""state":"learner""#)
    });
    let metrics = ok(leader, "GET", "/metrics", "");
    assert!(metrics.contains("big_cluster_balance_admits_total 1"), "{metrics}");
}

/// **A node that still holds a range is not removed**, because removing it removes the only
/// copy of what it holds and leaves the map naming a node nobody talks to.
#[test]
fn a_node_that_still_holds_a_range_cannot_be_removed() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    let leader = leader_of(&[a, b, spare]);
    let (status, body) = send(leader, "DELETE", "/admin/cluster/node?name=b", "");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("still holds shards 64.."), "{body}");
    assert!(body.contains("drain it first"), "{body}");
}

/// A draining node keeps answering: it still votes and still coordinates, so the one address a
/// client is holding does not go dark halfway through a scale-in.
#[test]
fn a_draining_node_still_answers_for_what_it_has_not_handed_over() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    );
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", &format!("amount {} 5\n", 70 * (1 << 20)));

    let leader = leader_of(&[a, b, spare]);
    ok(leader, "POST", "/admin/cluster/drain?name=b", "");
    until("the drain to be recorded", || {
        ok(a, "GET", "/cluster/topology", "").contains(r#""state":"draining""#)
    });

    // Still serving its range, and still usable as the node a client happens to reach.
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":1}"#);
    assert_eq!(ok(b, "POST", "/table/tx/query", "Count(All())"), r#"{"count":1}"#);
}

// -------------------------------------------------------------------------------------------
// Moving a populated range
//
// `docs/clustering.md` left this open with a question rather than an answer: *what does a query
// do while a shard is in flight*. It does nothing different. The source serves the range right
// up to the instant the handover commits, which is the same atomic act as a failover; the only
// thing denied anywhere is a write to that one range, for the length of the final pass.
// -------------------------------------------------------------------------------------------

/// A three-node file in which `a` and `b` own a range each and `a-spare` copies `a`.
fn three(a: SocketAddr, b: SocketAddr, spare: SocketAddr) -> String {
    format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"a-spare\"\naddr = \"{spare}\"\nreplica = \"a\"\n"
    )
}

/// **The records arrive, the answer never changes, and the old copy goes.**
#[test]
fn a_populated_range_moves_to_another_node_and_reads_never_stop() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");
    // Two records low (a's range) and two high (b's range).
    let hi = 70 * (1 << 20);
    ok(a, "POST", "/table/tx/import", "amount 1 5\ncountry 1 GB\namount 2 7\ncountry 2 FR\n");
    ok(
        a,
        "POST",
        "/table/tx/import",
        &format!("amount {hi} 11\ncountry {hi} GB\namount {} 13\ncountry {} US\n", hi + 1, hi + 1),
    );
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":4}"#);
    let sum_before = ok(a, "POST", "/table/tx/query", "Sum(All(), field=\"amount\")");

    // Move `b`'s populated range to `a`, which already serves one of its own.
    let leader = leader_of(&[a, b, spare]);
    let body = ok(leader, "POST", "/admin/cluster/move?range=1&to=a", "");
    assert!(body.contains(r#""outcome":"moved""#), "{body}");
    assert!(body.contains(r#""from":"b""#), "{body}");
    assert!(body.contains(r#""dropped":true"#), "{body}");

    until("every node to see the new owner", || {
        [a, b, spare].iter().all(|p| {
            ok(*p, "GET", "/cluster/topology", "").contains(r#""shards":"64..","primary":"a""#)
        })
    });

    // **Nothing about the answer changed.** Not the count, not the sum, and not the row keys -
    // which travel with the range, or a `GroupBy` would come back with nulls where names were.
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":4}"#);
    assert_eq!(ok(b, "POST", "/table/tx/query", "Count(All())"), r#"{"count":4}"#);
    assert_eq!(ok(b, "POST", "/table/tx/query", "Sum(All(), field=\"amount\")"), sum_before);
    let groups = ok(b, "POST", "/table/tx/query", "GroupBy(All(), field=\"country\")");
    assert!(groups.contains("US"), "the keys came across with the bits: {groups}");

    // `b` holds nothing now, so it can be taken out of the cluster.
    ok(leader, "POST", "/admin/cluster/drain?name=b", "");
    let (status, said) = send(leader, "DELETE", "/admin/cluster/node?name=b", "");
    assert_eq!(status, 200, "{said}");
}

/// **The failure a move must never have.** A write made while the range is in flight either
/// lands or is refused - it is never accepted by a node that is about to stop being asked.
///
/// This is the regression test for the design that lost one: dual-writing to the target during
/// the seed looks like it makes the final pass short, and instead lets a whole-fragment copy
/// discard an acknowledged write with equal counts on both sides afterwards.
#[test]
fn a_write_during_a_move_is_never_lost() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    let hi = 70 * (1 << 20);
    ok(a, "POST", "/table/tx/import", &format!("amount {hi} 1\n"));

    // Writes into the moving range, from another thread, for the whole of the move.
    let stop = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let writer = {
        let (stop, accepted) = (Arc::clone(&stop), Arc::clone(&accepted));
        std::thread::spawn(move || {
            let mut record = hi + 1;
            while !stop.load(Ordering::Relaxed) {
                let (status, _) =
                    send(a, "POST", "/table/tx/import", &format!("amount {record} 1\n"));
                // A refusal is a correct outcome: the range is mid-cutover. What must never
                // happen is a 200 for a write that is then not there.
                if status == 200 {
                    accepted.fetch_add(1, Ordering::Relaxed);
                }
                record += 1;
            }
        })
    };

    let leader = leader_of(&[a, b, spare]);
    let body = ok(leader, "POST", "/admin/cluster/move?range=1&to=a", "");
    assert!(body.contains(r#""outcome":"moved""#), "{body}");

    stop.store(true, Ordering::Relaxed);
    writer.join().expect("the writer thread");

    // **Every write that was told it landed is there.** One for the seed record, plus every
    // one the writer was given a 200 for.
    let expected = 1 + accepted.load(Ordering::Relaxed);
    // Every node routes from its own copy of the map, which it gets by replication - so a
    // count taken the instant the handover commits can still be answered from the old one.
    let mut last = String::new();
    for _ in 0..200 {
        last = ok(b, "POST", "/table/tx/query", "Count(All())");
        if last == format!("{{\"count\":{expected}}}") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        last,
        format!("{{\"count\":{expected}}}"),
        "every acknowledged write survived the move; a says {}",
        ok(a, "POST", "/table/tx/query", "Count(All())")
    );
}

/// A move that is abandoned leaves the range exactly where it was. Nothing was ever read from
/// the target, so there is nothing to undo.
#[test]
fn a_cancelled_move_leaves_the_range_where_it_was() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    let leader = leader_of(&[a, b, spare]);
    // A move to a node that is not in the cluster never starts.
    let (status, said) = send(leader, "POST", "/admin/cluster/move?range=1&to=nowhere", "");
    assert_eq!(status, 409, "{said}");
    assert!(said.contains("no node called `nowhere`"), "{said}");

    // And cancelling one that is not running says so rather than pretending.
    let (status, said) = send(leader, "POST", "/admin/cluster/cancel?range=1", "");
    assert_eq!(status, 409, "{said}");
    assert!(said.contains("not being moved"), "{said}");

    let after = ok(a, "GET", "/cluster/topology", "");
    assert!(after.contains(r#""shards":"64..","primary":"b""#), "{after}");
}

/// **Switched on, the balancer needs no operator.** The same step the test below asks for by
/// hand, taken by the steward on the agreement's leader - and only there - because the policy
/// says it may. Draining `b` is the only instruction anybody gives.
#[test]
fn the_balancer_runs_by_itself_when_switched_on() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let _a = start_balancing(&file, "a", a);
    let _b = start_balancing(&file, "b", b);
    let _spare = start_balancing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", &format!("amount {} 5\n", 70 * (1 << 20)));

    let leader = leader_of(&[a, b, spare]);
    ok(leader, "POST", "/admin/cluster/drain?name=b", "");
    until("the steward to move the range off the draining node", || {
        !ok(a, "GET", "/cluster/topology", "").contains(r#""shards":"64..","primary":"b""#)
    });
    // The records came with it.
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":1}"#);

    // **And it stops.** `b` holds nothing, so the next pass finds nothing to do, and a
    // draining node is never a destination - which is exactly the state in which it can be
    // removed for good. A removal refused is one the move has not finished landing.
    until("the move to be over and `b` removable", || {
        send(leader, "DELETE", "/admin/cluster/node?name=b", "").0 == 200
    });
}

/// The steward is stopped the way the workers are, and no slower. A node asked to stand down
/// comes back within a wake of the steward's sleep, not a whole period of it.
#[test]
fn a_node_stands_down_promptly_with_its_steward() {
    let (file, ports) = a_group_of_three();
    let mut a = start_balancing(&file, "a", ports[0]);
    until("the node to answer", || send(ports[0], "GET", "/ready", "").0 == 200);

    let started = std::time::Instant::now();
    a.close();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "stood down in {:?}",
        started.elapsed()
    );
}

/// **The balancer, over real nodes.** It decides from what the nodes actually weigh, and one
/// call does one thing - so a cluster that needs several steps takes several calls, each
/// against facts gathered afresh.
#[test]
fn rebalancing_takes_a_range_off_a_draining_node_and_then_stops() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", &format!("amount {} 5\n", 70 * (1 << 20)));

    let leader = leader_of(&[a, b, spare]);
    // Nothing to do while every node is where it should be.
    assert_eq!(
        ok(leader, "POST", "/admin/cluster/rebalance?force=true", ""),
        r#"{"did":null}"#,
        "a cluster nobody has asked to change is left alone"
    );

    // An operator asks for `b` to go. That outranks anything the balancer noticed by itself.
    ok(leader, "POST", "/admin/cluster/drain?name=b", "");
    let did = ok(leader, "POST", "/admin/cluster/rebalance?force=true", "");
    assert!(did.contains("moved shards 64.."), "{did}");

    until("the range to land somewhere else", || {
        !ok(a, "GET", "/cluster/topology", "").contains(r#""shards":"64..","primary":"b""#)
    });
    // The records came with it.
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":1}"#);

    // **And it stops.** `b` holds nothing now, so there is nothing left to take off it - and a
    // draining node is never a destination, so nothing is sent back.
    let again = ok(leader, "POST", "/admin/cluster/rebalance?force=true", "");
    assert_eq!(again, r#"{"did":null}"#);

    // The step was counted where an operator scrapes, and the move is over rather than
    // lingering as a range marked moving - the state that silences every step after it.
    let metrics = ok(leader, "GET", "/metrics", "");
    assert!(metrics.contains("big_cluster_balance_moves_total 1"), "{metrics}");
    assert!(metrics.contains("big_cluster_ranges_moving 0"), "{metrics}");
    assert!(metrics.contains("big_cluster_balance_drops_failed_total 0"), "{metrics}");

    // Which is exactly the state in which it can be removed for good.
    let (status, said) = send(leader, "DELETE", "/admin/cluster/node?name=b", "");
    assert_eq!(status, 200, "{said}");
}

/// **A peer that does not say what it weighs is counted, not just skipped.** To the balancer
/// that node is neither a source nor a destination, so a cluster with one silent member is a
/// cluster that quietly stops reshaping - and the only way an operator tells that apart from a
/// cluster with nothing to do is this number.
#[test]
fn a_peer_that_does_not_answer_the_balancer_is_counted() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let mut nodes = [
        start_agreeing(&file, "a", a),
        start_agreeing(&file, "b", b),
        start_agreeing(&file, "a-spare", spare),
    ];
    until("an elected leader", || ready(a).contains(r#""leader":""#));
    let leader = leader_of(&[a, b, spare]);

    // Take away a node that is not deciding, so the one that is keeps deciding.
    let silent = [a, b, spare].into_iter().position(|p| p != leader).expect("two others");
    nodes[silent].close();

    ok(leader, "POST", "/admin/cluster/rebalance?force=true", "");
    let metrics = ok(leader, "GET", "/metrics", "");
    assert!(metrics.contains("big_cluster_peer_load_unanswered_total 1"), "{metrics}");
}

/// Off unless asked. A cluster that reshapes itself unasked is one whose shape an operator
/// cannot predict, so the policy ships disabled and `?force=true` is what an operator uses.
#[test]
fn the_balancer_does_nothing_until_it_is_switched_on() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/import", &format!("amount {} 5\n", 70 * (1 << 20)));

    let leader = leader_of(&[a, b, spare]);
    assert_eq!(ok(leader, "POST", "/admin/cluster/rebalance", ""), r#"{"did":null}"#);
}

/// **The agreement gives the row-key namespace away when its holder dies, and nothing is
/// handed the same meaning twice.**
///
/// Four claims, and the middle two are what make the first one safe. The namespace moves. In
/// the gap before the successor holds every key, a write with a *new* key is refused rather
/// than given an id somebody else might also be giving out. Once it is ready, a key that
/// already had a row id has the *same* one - which is the proof that the keys came across from
/// the survivors rather than being invented again. And no record id is reused across the whole
/// thing.
#[test]
fn the_agreement_moves_the_namespace_when_its_holder_dies() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let (timing, leases) = brisk_electing();
    let mut a_node = start_with(&file, "a", a, timing, leases, None);
    let _b = start_with(&file, "b", b, timing, leases, None);
    let _spare = start_with(&file, "a-spare", spare, timing, leases, None);
    until("an elected leader", || ready(b).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");
    ok(a, "POST", "/sql", "INSERT INTO tx (amount, country) VALUES (5, 'GB')");
    ok(a, "POST", "/sql", "INSERT INTO tx (amount, country) VALUES (7, 'FR')");

    // What `GB` means before the handover. It has to mean the same afterwards: a successor
    // that invented a second row id for it would answer a `GroupBy` with two groups that are
    // one group, and nothing downstream could tell.
    let before = ok(b, "POST", "/table/tx/query", "GroupBy(All(), field=\"country\")");
    assert!(before.contains("GB") && before.contains("FR"), "{before}");

    // The schema leader dies. Nobody is told; nothing is edited.
    a_node.close();

    // 1. It moves, and to a node that is answering.
    until("the namespace to move", || {
        let seen = ok(b, "GET", "/cluster/topology", "");
        seen.contains(r#""schema_leader":"b""#) || seen.contains(r#""schema_leader":"a-spare""#)
    });

    // 2. Once the successor holds the keys, an existing key resolves to the id it always had.
    until("the successor to finish taking it over", || {
        send(b, "POST", "/sql", "INSERT INTO tx (amount, country) VALUES (9, 'GB')").0 == 200
    });
    let after = ok(b, "POST", "/table/tx/query", "GroupBy(All(), field=\"country\")");
    assert_eq!(
        after.matches("GB").count(),
        1,
        "one group for `GB`, not two - the keys came across: {after}"
    );

    // 3. A key nobody has ever seen works too, now that somebody holds the namespace.
    ok(b, "POST", "/sql", "INSERT INTO tx (amount, country) VALUES (11, 'DE')");

    // 4. No record id was handed out twice: four inserts, four records.
    until("both survivors to agree on the count", || {
        [b, spare]
            .iter()
            .all(|p| send(*p, "POST", "/table/tx/query", "Count(All())").1 == r#"{"count":4}"#)
    });

    // And the node that was deposed is marked behind: it may hold row ids it interned for
    // writes that never landed anywhere, so it must be repaired before it is trusted again.
    assert!(ready(b).contains(r#""behind":["a"]"#), "{}", ready(b));
}

/// **A record id promised by a leader that died is never promised again.**
///
/// The floor a leader allocates from lives in its memory, and a successor - here the same node,
/// restarted with an empty database - would start from what is on disk and hand out ids the
/// old leader had already given to writes that may not have landed. So the leader commits a
/// ceiling through the agreement before it hands out anything under it, in blocks, and a
/// fresh leader starts at the ceiling. The tell is the id: it jumps to the block boundary
/// rather than continuing from the highest record anybody holds.
#[test]
fn a_restarted_schema_leader_starts_above_everything_it_may_have_promised() {
    let dir = tempfile::tempdir().unwrap();
    let (file, ports) = a_group_of_three();
    let state = |name: &str| Some(dir.path().join(format!("{name}.raft")));
    let (timing, leases) = brisk();

    let mut a = start_with(&file, "a", ports[0], timing, leases, state("a"));
    let _b = start_with(&file, "b", ports[1], timing, leases, state("b"));
    let _c = start_with(&file, "c", ports[2], timing, leases, state("c"));
    until("an elected leader", || ready(ports[1]).contains(r#""leader":""#));

    ok(ports[1], "POST", "/table/tx", "");
    ok(ports[1], "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(ports[1], "POST", "/sql", "INSERT INTO tx (amount) VALUES (5)");
    ok(ports[1], "POST", "/sql", "INSERT INTO tx (amount) VALUES (7)");
    assert_eq!(
        ok(ports[1], "GET", "/table/tx/records?limit=5", ""),
        r#"{"records":[0,1],"next":null}"#,
        "two ids, from zero, as always"
    );

    // The schema leader goes away and comes back with its agreement state and nothing else -
    // a replaced disk. Its floor was in memory; the ceiling it committed was not.
    a.close();
    until("the copy that went away to be marked behind", || {
        ready(ports[1]).contains(r#""behind":["a"]"#)
    });
    let _a_again = start_with(&file, "a", ports[0], timing, leases, state("a"));
    until("the restarted node to answer", || send(ports[0], "GET", "/ready", "").0 == 200);
    // Caught up first: it is still a copy of the range every write goes to, and a copy with
    // no table in it refuses the write. That is the repair's job, not this test's subject.
    until("the restarted copy to be repaired", || {
        send(ports[1], "POST", "/repair", "").1.contains(r#""outcome":"caught up""#)
    });
    until("the leader to allocate again", || {
        send(ports[1], "POST", "/sql", "INSERT INTO tx (amount) VALUES (9)").0 == 200
    });

    // Not 2. The highest record anybody holds is 1, and a leader starting from there would be
    // free to re-issue whatever the old one promised in between.
    let ids = ok(ports[1], "GET", "/table/tx/records?after=1&limit=5", "");
    assert!(ids.contains("65536"), "the next id starts at the committed block: {ids}");
}

/// **A schema leader that loses the agreement stops assigning row ids**, even when the range
/// it holds is not fenced. A range with no copy is never fenced because nothing could take it;
/// the namespace can always be given to somebody else, so the node that holds it acts only
/// while it can prove it still does. What it costs is exactly one thing: a write with a key
/// nobody has seen waits. A write whose keys are known lands as it always did.
#[test]
fn the_schema_leader_stops_interning_when_it_loses_the_agreement() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    // `a` leads the schema and holds a range of one copy; the copy that makes the agreement
    // run at all is of `b`'s range. So when `b` and its copy go, `a` keeps serving its own
    // range - and cannot know whether the namespace is still its.
    let file = format!(
        "schema_leader = \"a\"\n\
         [[node]]\nname = \"a\"\naddr = \"{a}\"\nshards = \"0..64\"\n\
         [[node]]\nname = \"b\"\naddr = \"{b}\"\nshards = \"64..\"\n\
         [[node]]\nname = \"b-spare\"\naddr = \"{spare}\"\nreplica = \"b\"\n"
    );
    let _a = start_agreeing(&file, "a", a);
    let mut b_node = start_agreeing(&file, "b", b);
    let mut spare_node = start_agreeing(&file, "b-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");
    ok(a, "POST", "/table/tx/import", "amount 1 5\ncountry 1 GB\n");

    b_node.close();
    spare_node.close();

    // A key nobody has seen needs the leader, and `a` can no longer prove it is one. Whether
    // `a` led the agreement or followed it makes no difference: a leader's lease is renewed
    // by a majority it no longer has, a follower's by a leader it no longer hears.
    //
    // A fresh key every time. The lease outlives the closes by a moment, and a key interned
    // in that moment is a key this node knows from then on - a second attempt with the same
    // one would need no leader and prove nothing.
    let mut attempt = 100u64;
    until("the schema lease to run out", || {
        attempt += 1;
        let body = format!("amount {attempt} 7\ncountry {attempt} K{attempt}\n");
        let (status, body) = send(a, "POST", "/table/tx/import", &body);
        status == 503 && body.contains(r#""code":"schema_lease_lost""#)
    });

    // Still serving its own range: that range has no copy, so nothing could have taken it,
    // and the lease it lost is the namespace's and not the range's.
    assert!(ready(a).contains(r#""serving":true"#), "{}", ready(a));

    // A key it already knows needs nobody. The range is its own, and the write lands. (A
    // count over the whole table would fan out to `b`'s range, which is the part of the
    // cluster that is actually gone - so the import's own answer is the evidence.)
    let (status, body) = send(a, "POST", "/table/tx/import", "amount 3 9\ncountry 3 GB\n");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""imported":2"#), "{body}");
}

/// **The row-key namespace moves, and nothing is handed the same id twice.**
///
/// This is the one change that corrupts rather than fails. A successor that started from what
/// is on disk would re-issue record ids the old leader had already given away, and would invent
/// a second row id for a string that already had one - which the engine refuses outright, so
/// what it looks like afterwards is a write that will never land on a key nobody can see is
/// duplicated.
#[test]
fn the_schema_leader_moves_without_reissuing_anything() {
    let (a, b, spare) = (free_port(), free_port(), free_port());
    let file = three(a, b, spare);
    let _a = start_agreeing(&file, "a", a);
    let _b = start_agreeing(&file, "b", b);
    let _spare = start_agreeing(&file, "a-spare", spare);
    until("an elected leader", || ready(a).contains(r#""leader":""#));

    ok(a, "POST", "/table/tx", "");
    ok(a, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "");
    ok(a, "POST", "/table/tx/field/country?kind=set", "");
    ok(a, "POST", "/sql", "INSERT INTO tx (amount, country) VALUES (5, 'GB')");
    ok(a, "POST", "/sql", "INSERT INTO tx (amount, country) VALUES (7, 'FR')");
    assert_eq!(ok(a, "POST", "/table/tx/query", "Count(All())"), r#"{"count":2}"#);

    let leader = leader_of(&[a, b, spare]);
    assert!(ok(a, "GET", "/cluster/topology", "").contains(r#""schema_leader":"a""#));

    let body = ok(leader, "POST", "/admin/cluster/schema-leader?to=b", "");
    assert!(body.contains(r#""schema_leader":"b""#), "{body}");
    until("every node to know who leads the schema", || {
        [a, b, spare]
            .iter()
            .all(|p| ok(*p, "GET", "/cluster/topology", "").contains(r#""schema_leader":"b""#))
    });

    // **The keys came across.** A key `b` had never seen would be given a second row id, and
    // the group below would come back with a name missing.
    let groups = ok(a, "POST", "/table/tx/query", "GroupBy(All(), field=\"country\")");
    assert!(groups.contains("GB") && groups.contains("FR"), "{groups}");

    // **And so did the floor.** Ids keep going up rather than starting again over records that
    // are already there.
    ok(a, "POST", "/sql", "INSERT INTO tx (amount, country) VALUES (9, 'US')");
    ok(b, "POST", "/sql", "INSERT INTO tx (amount, country) VALUES (11, 'GB')");
    assert_eq!(
        ok(a, "POST", "/table/tx/query", "Count(All())"),
        r#"{"count":4}"#,
        "four inserts, four records - nothing overwrote anything"
    );
    let after = ok(b, "POST", "/table/tx/query", "GroupBy(All(), field=\"country\")");
    assert!(after.contains("US"), "a key invented after the handover works too: {after}");

    // **And the old leader refuses to intern**, rather than answering a coordinator that still
    // holds the map from before the move. Two nodes interning at once is the one failure that
    // hands one string two row ids, and this refusal is what keeps it to one: it used to be
    // assumed rather than checked, because the leader was a name in a file.
    let fingerprint =
        ClusterFile::parse(&file).unwrap().for_node(Some("a"), "").unwrap().fingerprint();
    let stale = |leader: usize| {
        big_cluster::wire::InternRequest {
            table: "tx".to_string(),
            field: "country".to_string(),
            keys: vec!["DE".to_string()],
            led: Some(big_cluster::wire::Led { epoch: 1, leader }),
        }
        .encode()
    };
    // Sent to `a`, which by its own map no longer leads.
    let (status, body) = send_bytes(a, "/internal/intern", &stale(0), fingerprint);
    assert_eq!(status, 503, "{}", String::from_utf8_lossy(&body));
    assert!(
        String::from_utf8_lossy(&body).contains("not_schema_leader"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    // Sent to `b`, which leads - by a sender that believes `a` does. Refused too: the answer
    // names who leads so the sender can ask the right node on purpose rather than by luck.
    let (status, body) = send_bytes(b, "/internal/intern", &stale(0), fingerprint);
    assert_eq!(status, 503, "{}", String::from_utf8_lossy(&body));
    assert!(
        String::from_utf8_lossy(&body).contains("not_schema_leader"),
        "{}",
        String::from_utf8_lossy(&body)
    );
}
