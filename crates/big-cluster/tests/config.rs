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

//! The cluster file, and every way it is allowed to be refused.
//!
//! Ownership by configuration has no protocol behind it, so the file *is* the agreement.
//! Every test here is about a disagreement being a startup failure that names what to change,
//! rather than a rule that resolves it into two nodes each answering half a query.

use big_cluster::{ClusterConfig, ClusterFile, ConfigError};

const TWO: &str = r#"
# The shape from docs/clustering.md.
schema_leader = "a"

[[node]]
name   = "a"
addr   = "10.0.0.1:7654"
shards = "0..64"

[[node]]
name   = "b"
addr   = "10.0.0.2:7654"
shards = "64.."
"#;

#[test]
fn a_file_names_its_nodes_and_their_ranges() {
    let config = ClusterFile::parse(TWO).unwrap().for_node(Some("a"), "").unwrap();
    assert_eq!(config.nodes().len(), 2);
    assert_eq!(config.this().name, "a");
    assert_eq!(config.leader().name, "a");
    assert!(config.leads_schema());
    assert!(!config.owns_everything());
}

/// The lookup everything else leans on: a pure function of the file, total by construction.
#[test]
fn every_shard_has_exactly_one_owner() {
    let config = ClusterFile::parse(TWO).unwrap().for_node(Some("b"), "").unwrap();
    assert_eq!(config.nodes()[config.owner(0)].name, "a");
    assert_eq!(config.nodes()[config.owner(63)].name, "a");
    assert_eq!(config.nodes()[config.owner(64)].name, "b");
    assert_eq!(config.nodes()[config.owner(u64::MAX)].name, "b");
}

/// A record id names its shard, and the client picked the record id. Placement needs no
/// coordination because of this line.
#[test]
fn a_record_id_names_its_owner() {
    let config = ClusterFile::parse(TWO).unwrap().for_node(Some("a"), "").unwrap();
    let width = 1u64 << 20;
    assert_eq!(config.nodes()[config.owner_of_record(0)].name, "a");
    assert_eq!(config.nodes()[config.owner_of_record(64 * width - 1)].name, "a");
    assert_eq!(config.nodes()[config.owner_of_record(64 * width)].name, "b");
}

#[test]
fn an_overlap_is_refused_and_names_both_nodes() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0..64"
[[node]]
name = "b"
addr = "2:2"
shards = "32.."
"#;
    let e = ClusterFile::parse(text).unwrap_err();
    assert!(matches!(e, ConfigError::Overlap { .. }), "{e:?}");
    let said = e.to_string();
    assert!(said.contains('a') && said.contains('b'), "{said}");
    assert!(said.contains("32"), "{said}");
}

#[test]
fn a_gap_between_two_ranges_is_refused() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0..64"
[[node]]
name = "b"
addr = "2:2"
shards = "65.."
"#;
    let e = ClusterFile::parse(text).unwrap_err();
    assert_eq!(e, ConfigError::Gap { from: 64, to: Some(65) });
}

/// The subtle one. Two ranges that meet exactly still leave the whole space above them
/// unowned, and a record id up there would be written by a client and read back by nobody.
#[test]
fn a_range_that_stops_short_of_the_end_is_a_gap() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0..64"
[[node]]
name = "b"
addr = "2:2"
shards = "64..128"
"#;
    let e = ClusterFile::parse(text).unwrap_err();
    assert_eq!(e, ConfigError::Gap { from: 128, to: None });
    assert!(e.to_string().contains("128.."), "{e}");
}

#[test]
fn the_space_below_the_first_range_is_a_gap_too() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "1.."
"#;
    assert_eq!(ClusterFile::parse(text).unwrap_err(), ConfigError::Gap { from: 0, to: Some(1) });
}

#[test]
fn a_leader_that_is_not_a_node_is_refused() {
    let text = r#"
schema_leader = "c"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
"#;
    assert_eq!(
        ClusterFile::parse(text).unwrap_err(),
        ConfigError::UnknownLeader { name: "c".to_string() }
    );
}

#[test]
fn a_file_with_no_leader_is_refused() {
    let text = r#"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
"#;
    assert_eq!(ClusterFile::parse(text).unwrap_err(), ConfigError::NoLeader);
}

#[test]
fn two_nodes_may_not_share_a_name_or_an_address() {
    let same_name = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0..1"
[[node]]
name = "a"
addr = "2:2"
shards = "1.."
"#;
    assert!(matches!(
        ClusterFile::parse(same_name).unwrap_err(),
        ConfigError::DuplicateNode { .. }
    ));

    let same_addr =
        same_name.replace("name = \"a\"\naddr = \"2:2\"", "name = \"b\"\naddr = \"1:1\"");
    assert!(matches!(
        ClusterFile::parse(&same_addr).unwrap_err(),
        ConfigError::DuplicateAddr { .. }
    ));
}

/// A typo is a silent misconfiguration everywhere else and a startup error here.
#[test]
fn an_unknown_key_is_refused_rather_than_ignored() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shard = "0.."
"#;
    let e = ClusterFile::parse(text).unwrap_err();
    assert!(matches!(e, ConfigError::UnknownKey { ref key, .. } if key == "shard"), "{e:?}");
}

#[test]
fn a_range_that_is_not_one_is_refused() {
    for bad in ["0", "64..0", "..64", "a..b", "0..64..128"] {
        let text = format!(
            "schema_leader = \"a\"\n[[node]]\nname = \"a\"\naddr = \"1:1\"\nshards = \"{bad}\"\n"
        );
        let e = ClusterFile::parse(&text).unwrap_err();
        assert!(matches!(e, ConfigError::BadRange { .. }), "`{bad}` gave {e:?}");
    }
}

/// A daemon has two ways to answer "which of these am I", and both failing is a refusal
/// rather than a guess.
#[test]
fn a_node_identifies_itself_by_name_or_by_address() {
    let file = ClusterFile::parse(TWO).unwrap();
    assert_eq!(file.clone().for_node(None, "10.0.0.2:7654").unwrap().this().name, "b");

    let e = ClusterFile::parse(TWO).unwrap().for_node(None, "10.0.0.9:7654").unwrap_err();
    assert!(matches!(e, ConfigError::Unplaced { .. }), "{e:?}");
    assert!(e.to_string().contains("--node"), "{e}");

    let e = ClusterFile::parse(TWO).unwrap().for_node(Some("z"), "").unwrap_err();
    assert_eq!(e, ConfigError::UnknownNode { name: "z".to_string() });
}

/// The un-clustered configuration is the general one with no peers, not a different shape.
#[test]
fn one_node_owns_everything_and_leads_the_schema() {
    let config = ClusterConfig::solo("127.0.0.1:7654");
    assert!(config.owns_everything());
    assert!(config.leads_schema());
    assert_eq!(config.owner(u64::MAX), 0);
}

#[test]
fn a_comment_inside_a_quoted_value_is_not_a_comment() {
    let text = r#"
schema_leader = "a"
peer_ca_file = "/etc/big/ca#1.pem"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
"#;
    assert_eq!(ClusterFile::parse(text).unwrap().peer_ca_file(), Some("/etc/big/ca#1.pem"));
}

// -------------------------------------------------------------------------------------------
// Replicas
// -------------------------------------------------------------------------------------------

const REPLICATED: &str = r#"
schema_leader = "a"

[[node]]
name   = "a"
addr   = "10.0.0.1:7654"
shards = "0..64"

[[node]]
name    = "a-spare"
addr    = "10.0.0.3:7654"
replica = "a"

[[node]]
name   = "b"
addr   = "10.0.0.2:7654"
shards = "64.."
"#;

/// A replica holds exactly its primary's range, and the file does not say so twice.
#[test]
fn a_replica_takes_its_range_from_its_primary() {
    let config = ClusterFile::parse(REPLICATED).unwrap().for_node(Some("a-spare"), "").unwrap();
    assert_eq!(config.this().name, "a-spare");
    assert_eq!(config.this().shards.to_string(), "0..64");
    assert!(!config.this().is_primary());
    assert!(config.is_replicated());
}

/// `owner` is where a read goes, and a read never goes to a copy.
#[test]
fn a_read_never_lands_on_a_replica() {
    let config = ClusterFile::parse(REPLICATED).unwrap().for_node(Some("a"), "").unwrap();
    for shard in [0u64, 1, 63, 64, u64::MAX] {
        let owner = &config.nodes()[config.owner(shard)];
        assert!(owner.is_primary(), "shard {shard} resolved to `{}`", owner.name);
    }
    assert_eq!(config.primaries().count(), 2);
}

/// A write goes to every copy, primary first. The order is the whole protocol: there is no
/// election, so which copy is authoritative has to be a fact about the file.
#[test]
fn every_copy_of_a_range_is_named_primary_first() {
    let config = ClusterFile::parse(REPLICATED).unwrap().for_node(Some("a"), "").unwrap();
    let names = |shard| -> Vec<String> {
        config.copies(shard).into_iter().map(|i| config.nodes()[i].name.clone()).collect()
    };
    assert_eq!(names(0), vec!["a".to_string(), "a-spare".to_string()]);
    assert_eq!(names(64), vec!["b".to_string()]);
}

/// A replica does not add a range, so it cannot create an overlap - and its primary's range is
/// still checked against everybody else's.
#[test]
fn a_replica_is_not_an_overlap() {
    assert!(ClusterFile::parse(REPLICATED).is_ok());
}

#[test]
fn a_node_is_either_a_primary_or_a_copy_of_one() {
    let both = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
[[node]]
name = "a2"
addr = "2:2"
shards = "0.."
replica = "a"
[[node]]
name = "a3"
addr = "3:3"
replica = "a"
"#;
    assert!(matches!(
        ClusterFile::parse(both).unwrap_err(),
        ConfigError::BothShardsAndReplica { .. }
    ));

    let neither = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
[[node]]
name = "a2"
addr = "2:2"
"#;
    let e = ClusterFile::parse(neither).unwrap_err();
    assert!(matches!(e, ConfigError::MissingField { .. }), "{e:?}");
    // The message names the alternative, because a node with neither is usually a node that
    // was meant to have one of them.
    assert!(e.to_string().contains("replica"), "{e}");
}

#[test]
fn a_replica_of_something_that_is_not_there_is_refused() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
[[node]]
name = "a2"
addr = "2:2"
replica = "z"
[[node]]
name = "a3"
addr = "3:3"
replica = "a"
"#;
    assert!(matches!(ClusterFile::parse(text).unwrap_err(), ConfigError::UnknownPrimary { .. }));
}

/// There is no chain. A replica mirrors the node a read goes to, and a copy of a copy would
/// mean two hops of "which one is current" with no protocol to answer either.
#[test]
fn a_replica_of_a_replica_is_refused() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
[[node]]
name = "a2"
addr = "2:2"
replica = "a"
[[node]]
name = "a3"
addr = "3:3"
replica = "a2"
"#;
    let e = ClusterFile::parse(text).unwrap_err();
    assert!(matches!(e, ConfigError::ReplicaOfReplica { .. }), "{e:?}");
}

/// The leader decides what a row key means rather than copying the decision.
#[test]
fn the_schema_leader_may_not_be_a_replica() {
    let text = r#"
schema_leader = "a2"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
[[node]]
name = "a2"
addr = "2:2"
replica = "a"
[[node]]
name = "a3"
addr = "3:3"
replica = "a"
"#;
    assert_eq!(
        ClusterFile::parse(text).unwrap_err(),
        ConfigError::LeaderIsReplica { name: "a2".to_string() }
    );
}

/// Every node a copy of something means nothing owns anything, which the totality check would
/// otherwise report as a gap starting at zero.
#[test]
fn a_file_of_nothing_but_replicas_is_refused() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
replica = "b"
"#;
    assert_eq!(ClusterFile::parse(text).unwrap_err(), ConfigError::NoNodes);
}

/// **A cluster of two cannot fail over**, so a copy in one is a copy that can never be used -
/// and the survivor of a failure stops serving rather than risk being the second node to serve
/// one range. Refused here rather than discovered on the night it matters.
#[test]
fn a_replicated_cluster_of_two_is_refused() {
    let text = r#"
schema_leader = "a"
[[node]]
name = "a"
addr = "1:1"
shards = "0.."
[[node]]
name = "a-spare"
addr = "2:2"
replica = "a"
"#;
    let e = ClusterFile::parse(text).unwrap_err();
    assert_eq!(e, ConfigError::TooFewForFailover { nodes: 2 });
    assert!(e.to_string().contains("majority of two is two"), "{e}");

    // Two nodes with no copy between them is fine: nothing there was ever going to fail over.
    let unreplicated = text.replace("replica = \"a\"", "shards = \"1..\"").replace("0..", "0..1");
    assert!(ClusterFile::parse(&unreplicated).is_ok(), "{unreplicated}");
}

// -------------------------------------------------------------------------------------------
// What names a cluster
// -------------------------------------------------------------------------------------------

/// **The check that used to make joining impossible.** Two nodes of one cluster now legitimately
/// hold different files - the agreement decides who is a member - so the shape of the file
/// cannot be what they recognise each other by. `cluster_id` is what they use instead.
#[test]
fn two_nodes_with_different_files_and_one_cluster_id_recognise_each_other() {
    let three = r#"
cluster_id    = "orders-eu"
schema_leader = "a"

[[node]]
name   = "a"
addr   = "10.0.0.1:7654"
shards = "0..64"

[[node]]
name   = "b"
addr   = "10.0.0.2:7654"
shards = "64.."

[[node]]
name    = "a-spare"
addr    = "10.0.0.3:7654"
replica = "a"
"#;
    // The file a node joining later is given: it knows itself and one peer, and nothing about
    // the shape the cluster happens to have today.
    let joining = r#"
cluster_id    = "orders-eu"
schema_leader = "a"

[[node]]
name   = "a"
addr   = "10.0.0.1:7654"
shards = "0.."
"#;

    let old = ClusterFile::parse(three).unwrap().for_node(Some("a"), "").unwrap();
    let new = ClusterFile::parse(joining).unwrap().for_node(Some("a"), "").unwrap();
    assert_eq!(old.fingerprint(), new.fingerprint(), "same cluster, different files");
}

/// And a node pointed at the wrong cluster is still refused, which is what the check is for.
#[test]
fn a_different_cluster_id_is_a_different_cluster() {
    let one = "cluster_id = \"orders-eu\"\nschema_leader = \"a\"\n\
               [[node]]\nname = \"a\"\naddr = \"10.0.0.1:7654\"\nshards = \"0..\"\n";
    let two = "cluster_id = \"orders-us\"\nschema_leader = \"a\"\n\
               [[node]]\nname = \"a\"\naddr = \"10.0.0.1:7654\"\nshards = \"0..\"\n";
    let a = ClusterFile::parse(one).unwrap().for_node(Some("a"), "").unwrap();
    let b = ClusterFile::parse(two).unwrap().for_node(Some("a"), "").unwrap();
    assert_ne!(a.fingerprint(), b.fingerprint());
}

/// **Nothing that deploys the old way has to change.** Without an id, the shape of the file is
/// still what identifies the cluster - exact for the case that has always worked, which is
/// every node started from one file.
#[test]
fn a_file_with_no_cluster_id_is_still_identified_by_its_shape() {
    let text = "schema_leader = \"a\"\n\
                [[node]]\nname = \"a\"\naddr = \"10.0.0.1:7654\"\nshards = \"0..64\"\n\
                [[node]]\nname = \"b\"\naddr = \"10.0.0.2:7654\"\nshards = \"64..\"\n";
    let moved = "schema_leader = \"a\"\n\
                 [[node]]\nname = \"a\"\naddr = \"10.0.0.1:7654\"\nshards = \"0..32\"\n\
                 [[node]]\nname = \"b\"\naddr = \"10.0.0.2:7654\"\nshards = \"32..\"\n";
    let a = ClusterFile::parse(text).unwrap().for_node(Some("a"), "").unwrap();
    let b = ClusterFile::parse(text).unwrap().for_node(Some("b"), "").unwrap();
    let edited = ClusterFile::parse(moved).unwrap().for_node(Some("a"), "").unwrap();

    assert_eq!(a.fingerprint(), b.fingerprint(), "which node this is does not change it");
    assert_ne!(a.fingerprint(), edited.fingerprint(), "a file somebody edited does");
}

/// **Two files that disagree about who leads the schema still describe one cluster.**
///
/// The leader used to be folded into the shape, and that was right while it was a name read
/// once from a file. It moves at runtime now - by an operator, and by the agreement itself -
/// so a deployment whose files were updated at different moments after a *correct* failover
/// would have two nodes that could no longer talk: a successful failover causing a partition,
/// which is the worst failure on offer. The map overrides the file within a heartbeat, so
/// folding it in protected nothing.
#[test]
fn who_leads_the_schema_is_not_part_of_the_shape() {
    let with = |leader: &str| {
        format!(
            "schema_leader = \"{leader}\"\n\
             [[node]]\nname = \"a\"\naddr = \"10.0.0.1:7654\"\nshards = \"0..64\"\n\
             [[node]]\nname = \"b\"\naddr = \"10.0.0.2:7654\"\nshards = \"64..\"\n"
        )
    };
    let a = ClusterFile::parse(&with("a")).unwrap().for_node(Some("a"), "").unwrap();
    let b = ClusterFile::parse(&with("b")).unwrap().for_node(Some("a"), "").unwrap();
    assert_eq!(a.fingerprint(), b.fingerprint());

    // The key is still required, and still has to name a node that could hold it: it seeds the
    // map, and a fresh cluster has nothing else to start from.
    assert!(matches!(ClusterFile::parse(&with("nobody")), Err(ConfigError::UnknownLeader { .. })));
}

// ------------------------------------------------------------------------------------------
// Joining without a file
//
// A node that joins is given the cluster's id and one address, and takes the rest from the
// node it dials. What it builds has to be the same kind of thing a file produces, because
// everything downstream of startup knows only `ClusterConfig`.
// ------------------------------------------------------------------------------------------

/// The configuration a joining node builds is a *seed*: it names every peer so the agreement
/// can be reached, and claims no range for the node itself.
#[test]
fn a_joining_node_names_every_peer_and_owns_nothing() {
    let members = [
        ("a".to_string(), "10.0.0.1:7654".to_string()),
        ("b".to_string(), "10.0.0.2:7654".to_string()),
    ];
    let config = ClusterConfig::joining("big-demo", &members, "b").unwrap();

    assert_eq!(config.nodes().len(), 2, "every peer is reachable: {:?}", config.nodes());
    let me = config.this();
    assert_eq!(me.name, "b");
    assert!(me.replica_of.is_some(), "a joining node holds no range of its own");
}

/// **The id is what makes two nodes one cluster**, so a joining node computes the same
/// fingerprint as the cluster it dialled - which is the check every peer request carries.
#[test]
fn a_joining_node_matches_the_fingerprint_of_the_cluster_it_dialled() {
    let file = ClusterFile::parse(&format!("cluster_id = \"big-demo\"\n{TWO}")).unwrap();
    let from_file = file.for_node(Some("a"), "").unwrap();

    let members = [
        ("a".to_string(), "10.0.0.1:7654".to_string()),
        ("b".to_string(), "10.0.0.2:7654".to_string()),
    ];
    let joined = ClusterConfig::joining("big-demo", &members, "b").unwrap();

    assert_eq!(joined.fingerprint(), from_file.fingerprint(), "same cluster, same stamp");
}

/// **A node the cluster has never heard of is refused at startup, by name.** This is the whole
/// ergonomic point: the answer names the step that was skipped rather than leaving a daemon
/// that looks healthy and is talking to nobody.
#[test]
fn a_node_the_cluster_does_not_know_is_refused_with_the_step_it_is_missing() {
    let members = [("a".to_string(), "10.0.0.1:7654".to_string())];
    let e = ClusterConfig::joining("big-demo", &members, "b").unwrap_err();

    assert!(matches!(e, ConfigError::NotAMember { .. }), "{e:?}");
    let said = e.to_string();
    assert!(said.contains("add-node"), "it names the command that fixes it: {said}");
}
