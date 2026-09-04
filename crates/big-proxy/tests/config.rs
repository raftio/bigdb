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

//! The two readers of `cluster.toml`, held to agreeing where they overlap and to differing
//! where they are meant to.
//!
//! `big-proxy` reads the same file the daemon does and takes two fields out of it. That is a
//! duplicate, and a duplicate is a thing that drifts — so this pins both halves of the claim:
//! the same `(name, addr)` pairs for a file both accept, and a deliberate divergence for the
//! files only one of them should refuse.

use big_cluster::ClusterFile;
use big_proxy::config::parse_cluster;
use std::path::Path;

/// The file that ships in `deploy/cluster/`, read by both.
#[test]
fn both_readers_agree_on_the_shipped_cluster_file() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/cluster/cluster.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    let daemon = ClusterFile::parse(&text).expect("the shipped file is valid for the daemon");
    let proxy = parse_cluster(&text, "shipped").expect("and for the proxy");

    let daemon_pairs: Vec<(String, String)> =
        daemon.nodes().iter().map(|n| (n.name.clone(), n.addr.clone())).collect();
    let proxy_pairs: Vec<(String, String)> = proxy.into_iter().map(|n| (n.name, n.addr)).collect();

    assert_eq!(daemon_pairs, proxy_pairs, "the readers disagree about who is in the cluster");
    assert_eq!(daemon_pairs.len(), 3, "the shipped example is three nodes");
}

/// The divergence, pinned.
///
/// A shard gap makes the *daemon* answer part of a query and not say so, which is why it
/// refuses to start. It says nothing about whether a front door can reach those nodes — and a
/// proxy that refused here would turn a fixable misconfiguration into a total outage, taking
/// down the nodes that are correctly configured along with the ones that are not.
#[test]
fn a_shard_gap_stops_the_daemon_and_not_the_proxy() {
    let gappy = r#"
cluster_id = "x"
[[node]]
name   = "a"
addr   = "a:7654"
shards = "0..8"

[[node]]
name   = "b"
addr   = "b:7654"
shards = "64.."
"#;

    assert!(
        ClusterFile::parse(gappy).is_err(),
        "the daemon must still refuse a shard map with a hole in it — if this passes, the \
         divergence below is no longer a divergence and this reader could just use ClusterFile"
    );

    let proxy = parse_cluster(gappy, "gappy").expect("a front door has no view on shard maps");
    assert_eq!(proxy.len(), 2);
    assert_eq!(proxy[0].addr, "a:7654");
}

/// The same, for the other refusal an operator can hit: a copy needs three nodes to fail over,
/// and that is a fact about voting rather than about reachability.
#[test]
fn a_replica_without_a_majority_stops_the_daemon_and_not_the_proxy() {
    let two = r#"
[[node]]
name   = "a"
addr   = "a:7654"
shards = "0.."

[[node]]
name    = "a-spare"
addr    = "a-spare:7654"
replica = "a"
"#;

    assert!(ClusterFile::parse(two).is_err(), "the daemon needs three for a majority");
    assert_eq!(parse_cluster(two, "two").expect("the proxy does not vote").len(), 2);
}

/// What both must refuse: a file naming no nodes at all.
#[test]
fn neither_reader_accepts_a_file_with_no_nodes() {
    let empty = "cluster_id = \"x\"\nschema_leader = \"a\"\n";
    assert!(ClusterFile::parse(empty).is_err());
    assert!(parse_cluster(empty, "empty").is_err());
}
