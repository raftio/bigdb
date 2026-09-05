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
    assert_eq!(daemon_pairs.len(), 2, "the shipped example is two nodes, a range each");
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

// -------------------------------------------------------------------------------------------
// The Kubernetes manifests
// -------------------------------------------------------------------------------------------
//
// `deploy/k8s` ships two TOML documents inside one ConfigMap, and they are not
// interchangeable - which is exactly the mistake that is easy to make while editing YAML. The
// tests below pin the difference:
//
// - `seed.toml` is a **membership**: a node with no log counts a majority out of it, so it must
//   satisfy the daemon's reader, ranges and all.
// - `upstreams.toml` is an **address space**: it deliberately names pods that do not exist yet,
//   so that `kubectl scale` reaches a client with no proxy restart. Only this crate's reader
//   ever sees it, and it must accept it.
//
// A third file exists only at runtime: `join.sh` writes the cluster file a joining pod starts
// with, by appending a `replica` entry per name up to its own ordinal. That append is
// reproduced here, because a file the daemon refuses is a pod that crash-loops with no clue
// pointing back at a shell script in a ConfigMap.

/// One `key: |` block scalar out of a ConfigMap, dedented.
///
/// A parser rather than a YAML dependency: what is needed is one block, the indentation is
/// fixed at four spaces by the file itself, and a test that pulled in a YAML crate to read two
/// strings would be a dependency the shipped binary is measured against.
fn block(yaml: &str, key: &str) -> String {
    let opener = format!("  {key}: |");
    let start = yaml.find(&opener).unwrap_or_else(|| panic!("no `{key}:` block in the manifest"));
    let body = &yaml[start + opener.len()..];
    let mut out = String::new();
    for line in body.lines().skip(1) {
        if !line.is_empty() && !line.starts_with("    ") {
            break;
        }
        out.push_str(line.strip_prefix("    ").unwrap_or(""));
        out.push('\n');
    }
    out
}

fn manifest() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/k8s/10-config.yaml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn the_k8s_seed_is_a_membership_both_readers_accept() {
    let seed = block(&manifest(), "seed.toml");
    let daemon = ClusterFile::parse(&seed).expect("the seed is what a founding node starts from");
    let proxy = parse_cluster(&seed, "seed").expect("and the proxy could read it too");

    let names: Vec<&str> = daemon.nodes().iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["big-0", "big-1", "big-2"], "three, because a majority of two is two");
    assert_eq!(proxy.len(), 3);
    assert_eq!(proxy[0].addr, "big-0.nodes:7654", "the address is the headless Service's");
    assert_ne!(proxy[0].name, proxy[0].addr, "the name is what the certificate carries");
}

#[test]
fn the_k8s_upstream_list_may_name_pods_that_do_not_exist() {
    let upstreams = block(&manifest(), "upstreams.toml");
    let proxy = parse_cluster(&upstreams, "upstreams").expect("a front door does not vote");
    assert!(proxy.len() > 3, "the ceiling is above the founding size, or scaling needs a restart");

    // And the daemon must refuse it, which is why it is a separate file: every entry past the
    // third has no range, so this is not a shard map and was never meant to be seeded from.
    assert!(
        ClusterFile::parse(&upstreams).is_err(),
        "if the daemon accepts this, somebody has given the upstream list shard ranges - and a \
         node seeded from it would count a majority out of pods that do not exist"
    );
}

#[test]
fn a_joining_pods_generated_file_is_one_the_daemon_accepts() {
    // What `join.sh` writes for `big-4`: the seed, then every name up to its own ordinal that
    // the seed does not already have, as a replica of the first node.
    let mut file = block(&manifest(), "seed.toml");
    for i in 3..=4 {
        file.push_str(&format!(
            "\n[[node]]\nname    = \"big-{i}\"\naddr    = \"big-{i}.nodes:7654\"\nreplica = \"big-0\"\n"
        ));
    }

    let daemon = ClusterFile::parse(&file).expect("a joining node must be able to start");
    assert_eq!(daemon.nodes().len(), 5);
    // The roster is the point: the leader that dials this node may be any of them.
    let names: Vec<&str> = daemon.nodes().iter().map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"big-0") && names.contains(&"big-4"));
}
