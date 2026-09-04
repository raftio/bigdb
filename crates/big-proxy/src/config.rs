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

//! Where the node list comes from.
//!
//! This reads `cluster.toml`, and reads **less** of it than the daemon does. That is deliberate
//! rather than lazy, and it is the one place this crate knowingly duplicates something
//! `big-cluster` already has.
//!
//! `ClusterFile::parse` refuses a file whose shard ranges have a gap or an overlap, whose
//! replica names another replica, or which has fewer than three nodes while any of them is a
//! copy. Every one of those is a **daemon** problem: it is the daemon that would answer part of
//! a query and not say so. A front door that refused to start for any of them would turn a
//! misconfiguration somebody could still fix into a total outage — the nodes are up, the shard
//! map is wrong, and now nothing can reach the ones that are right either.
//!
//! So this takes `name` and `addr` and discards the rest. `shards`, `replica` and
//! `schema_leader` describe who owns what, and this proxy does not route by key.
//!
//! `tests/config.rs` asserts the two readers agree on the part they share, and that this one
//! accepts a file the daemon's refuses. `big-cluster` is a dev-dependency there, so the shipped
//! binary still cannot reach the engine.

use std::path::Path;

/// One node, as far as a front door is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAddr {
    /// Also the TLS server name: a node certificate carries `subjectAltName = DNS:<name>`.
    pub name: String,
    pub addr: String,
}

/// Read the `[[node]]` entries out of a cluster file.
///
/// The same hand-written TOML subset the daemon accepts: `[[node]]` tables, `key = "value"`
/// lines, `#` comments. **Unknown top-level keys are ignored rather than refused** — the daemon
/// may grow keys this proxy has no use for, and a front door that stopped for one would be
/// coupling itself to a file it only borrows two fields from.
pub fn read_cluster(path: &Path) -> Result<Vec<NodeAddr>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse_cluster(&text, &path.display().to_string())
}

pub fn parse_cluster(text: &str, origin: &str) -> Result<Vec<NodeAddr>, String> {
    let mut nodes: Vec<NodeAddr> = Vec::new();
    let mut in_node = false;
    let mut name: Option<String> = None;
    let mut addr: Option<String> = None;

    // A `[[node]]` ends where the next table starts or where the file does, so the entry being
    // built is flushed at both.
    let mut flush = |name: &mut Option<String>, addr: &mut Option<String>| -> Result<(), String> {
        let (Some(n), Some(a)) = (name.take(), addr.take()) else {
            return Err(format!("{origin}: a [[node]] is missing its name or addr"));
        };
        nodes.push(NodeAddr { name: n, addr: a });
        Ok(())
    };

    for (number, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if in_node {
                flush(&mut name, &mut addr)?;
            }
            in_node = line == "[[node]]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{origin}:{}: not a key = value line", number + 1));
        };
        if !in_node {
            // A top-level key. `cluster_id`, `schema_leader`, `peer_ca_file` — none of this
            // proxy's business, and neither is whatever is added next.
            continue;
        }
        let value = unquote(value.trim());
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "addr" => addr = Some(value.to_string()),
            // `shards`, `replica`, and anything later. Read and dropped: see the module doc.
            _ => {}
        }
    }
    if in_node {
        flush(&mut name, &mut addr)?;
    }

    if nodes.is_empty() {
        return Err(format!("{origin}: no [[node]] entries"));
    }
    let mut seen: Vec<&str> = Vec::new();
    for node in &nodes {
        if seen.contains(&node.name.as_str()) {
            return Err(format!("{origin}: two nodes named {}", node.name));
        }
        seen.push(&node.name);
    }
    Ok(nodes)
}

fn strip_comment(line: &str) -> &str {
    // A `#` inside a quoted value is not a comment. Values here are addresses and names, so
    // tracking the quote state is one bool rather than a parser.
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> &str {
    value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO: &str = r#"
# Three nodes: two ranges and a copy of one of them.
cluster_id      = "big-demo"
schema_leader   = "a"
peer_ca_file = "/run/big/peer-ca.pem"   # staged by the entrypoint

[[node]]
name   = "a"
addr   = "a:7654"
shards = "0..64"

[[node]]
name   = "b"
addr   = "b:7654"
shards = "64.."

[[node]]
name    = "a-spare"
addr    = "a-spare:7654"
replica = "a"
"#;

    #[test]
    fn it_reads_the_shipped_example() {
        let nodes = parse_cluster(DEMO, "demo").unwrap();
        assert_eq!(
            nodes,
            vec![
                NodeAddr { name: "a".into(), addr: "a:7654".into() },
                NodeAddr { name: "b".into(), addr: "b:7654".into() },
                NodeAddr { name: "a-spare".into(), addr: "a-spare:7654".into() },
            ]
        );
    }

    /// The divergence from the daemon, pinned: a shard map with a hole in it is the daemon's
    /// problem, and a front door that would not start for it is a second outage.
    #[test]
    fn a_shard_gap_is_not_this_readers_business() {
        let gappy = r#"
[[node]]
name = "a"
addr = "a:1"
shards = "0..8"

[[node]]
name = "b"
addr = "b:1"
shards = "64.."
"#;
        let nodes = parse_cluster(gappy, "gappy").unwrap();
        assert_eq!(nodes.len(), 2, "the gap between 8 and 64 is not a reason to refuse to start");
    }

    /// A copy with no third node is refused by the daemon and irrelevant here.
    #[test]
    fn a_lone_replica_is_not_this_readers_business_either() {
        let two = "[[node]]\nname = \"a\"\naddr = \"a:1\"\nshards = \"0..\"\n\n\
                   [[node]]\nname = \"s\"\naddr = \"s:1\"\nreplica = \"a\"\n";
        assert_eq!(parse_cluster(two, "two").unwrap().len(), 2);
    }

    #[test]
    fn an_unknown_top_level_key_is_ignored_not_refused() {
        let future = "something_added_next_year = \"x\"\n\n[[node]]\nname=\"a\"\naddr=\"a:1\"\n";
        assert_eq!(parse_cluster(future, "f").unwrap().len(), 1);
    }

    #[test]
    fn a_node_missing_a_field_names_the_problem() {
        let e = parse_cluster("[[node]]\nname = \"a\"\n", "x").unwrap_err();
        assert!(e.contains("name or addr"), "{e}");
        let e = parse_cluster("[[node]]\naddr = \"a:1\"\n", "x").unwrap_err();
        assert!(e.contains("name or addr"), "{e}");
    }

    #[test]
    fn an_empty_file_is_refused_because_a_proxy_needs_somewhere_to_go() {
        assert!(parse_cluster("cluster_id = \"x\"\n", "x").is_err());
        assert!(parse_cluster("", "x").is_err());
    }

    /// Two nodes with one name would make the `/ready` table and the logs ambiguous, and the
    /// name is the TLS server name besides.
    #[test]
    fn a_duplicate_name_is_refused() {
        let dup = "[[node]]\nname=\"a\"\naddr=\"a:1\"\n\n[[node]]\nname=\"a\"\naddr=\"b:1\"\n";
        let e = parse_cluster(dup, "dup").unwrap_err();
        assert!(e.contains("two nodes named a"), "{e}");
    }

    #[test]
    fn a_hash_inside_a_value_is_not_a_comment() {
        let nodes = parse_cluster("[[node]]\nname=\"a#b\"\naddr=\"a:1\"\n", "x").unwrap();
        assert_eq!(nodes[0].name, "a#b");
    }

    #[test]
    fn a_line_that_is_not_a_pair_names_its_line_number() {
        let e = parse_cluster("[[node]]\nname = \"a\"\nnonsense\n", "x").unwrap_err();
        assert!(e.contains(":3:"), "{e}");
    }
}
