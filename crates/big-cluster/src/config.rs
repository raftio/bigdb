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

//! Who owns which shards, read from a file at startup and never changed after it.
//!
//! `owner(shard)` is a range lookup and nothing else. There is no membership protocol here
//! and no consensus, so a file that disagrees with itself is a fact the operator has to fix -
//! which is why every disagreement below is a startup failure naming the shards involved,
//! rather than a rule that resolves it. Two nodes each believing they own shard 64 would each
//! answer half a query, and neither would notice.
//!
//! **The parser is not a TOML implementation.** It reads the subset this file is written in:
//! `[[node]]` tables, double-quoted string values, `#` comments. An unknown key is refused
//! rather than ignored, because the failure mode of ignoring one is a node silently owning a
//! range the operator did not write - `shard = "0..64"` for `shards` is a typo that costs a
//! silent misconfiguration everywhere else and a startup error here.

use big_engine::{RecordId, ShardId};

/// A half-open range of shard ids. **Defined in [`big_engine`]**, because a range is also what
/// a read can be scoped to and the storage layer cannot ask a crate above it what one is.
///
/// What lives here is only the part the storage layer has no use for: reading one out of a
/// cluster file.
pub use big_engine::ShardRange;

/// Parsing a range as an operator writes it, which is a cluster-file concern and nothing else's.
trait ParseRange: Sized {
    fn parse(text: &str) -> Option<Self>;
}

impl ParseRange for ShardRange {
    fn parse(text: &str) -> Option<Self> {
        let (lo, hi) = text.split_once("..")?;
        let start = lo.trim().parse().ok()?;
        let hi = hi.trim();
        let end = if hi.is_empty() { None } else { Some(hi.parse().ok()?) };
        // An empty range owns nothing and would leave a gap that the totality check would
        // then blame on the node after it. Refusing it here names the right node.
        if end.is_some_and(|e| e <= start) {
            return None;
        }
        Some(Self { start, end })
    }
}

/// One peer: what to call it, where to reach it, and what it holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Node {
    /// The name the file gave it, used in errors and in `schema_leader`.
    pub name: String,
    /// `host:port`, exactly as written. Resolved when a connection is made, not here: a node
    /// that is down at startup is not a configuration error.
    pub addr: String,
    /// The shards it holds. A replica holds exactly its primary's range, so this is filled in
    /// from the primary rather than written twice in the file - two places to say one thing is
    /// one place to say it differently.
    pub shards: ShardRange,
    /// The primary this node mirrors, if it is a replica. `None` means it is a primary itself.
    ///
    /// An index rather than a name: the name is resolved once, at startup, so that nothing
    /// downstream has to handle a name that turns out not to be there.
    pub replica_of: Option<usize>,
}

impl Node {
    /// Whether this node is the one a read goes to for its range.
    pub fn is_primary(&self) -> bool {
        self.replica_of.is_none()
    }
}

/// Why a cluster file was refused.
///
/// Every variant names what to change. A cluster file is written by hand and read once, so an
/// error that does not say which line or which shards is an error the operator has to bisect.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConfigError {
    Syntax {
        line: usize,
        why: &'static str,
    },
    UnknownKey {
        line: usize,
        key: String,
    },
    BadRange {
        line: usize,
        text: String,
    },
    NoNodes,
    DuplicateNode {
        name: String,
    },
    DuplicateAddr {
        addr: String,
    },
    MissingField {
        node: String,
        key: &'static str,
    },
    /// A node with both a range of its own and a primary to mirror. It is one or the other.
    BothShardsAndReplica {
        node: String,
    },
    /// `replica = "..."` naming something that is not in the file.
    UnknownPrimary {
        node: String,
        primary: String,
    },
    /// A replica of a replica. There is no chain here: a replica mirrors a primary, and a
    /// primary is the node a read goes to.
    ReplicaOfReplica {
        node: String,
        primary: String,
    },
    /// The schema leader is a replica. It owns the row-key namespace, which is a decision
    /// rather than a copy of one, so it has to be a node reads already go to.
    LeaderIsReplica {
        name: String,
    },
    /// A cluster with a copy in it, and too few nodes to ever use the copy.
    ///
    /// Failing over is a decision a majority has to agree on, and a majority of two is two -
    /// so a cluster of two cannot promote anything, and the survivor of a failure stops
    /// serving rather than risk being the second node to serve one range. That is strictly
    /// worse than the same two machines with no copy at all, which is why it is refused here
    /// rather than discovered on the night it matters.
    TooFewForFailover {
        nodes: usize,
    },
    Overlap {
        a: String,
        b: String,
        from: ShardId,
        to: ShardId,
    },
    Gap {
        from: ShardId,
        to: Option<ShardId>,
    },
    NoLeader,
    UnknownLeader {
        name: String,
    },
    /// `--node` named something the file does not contain.
    UnknownNode {
        name: String,
    },
    /// No `--node`, and the address this daemon was told to bind is not in the file either.
    /// Both ways of answering "which of these am I" failed, so it has to be told.
    Unplaced {
        addr: String,
    },
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Syntax { line, why } => write!(f, "line {line}: {why}"),
            Self::UnknownKey { line, key } => write!(
                f,
                "line {line}: unknown key `{key}`; a cluster file has name, addr and shards \
                 inside [[node]], and schema_leader and peer_ca_file outside it"
            ),
            Self::BadRange { line, text } => write!(
                f,
                "line {line}: `{text}` is not a shard range; write `0..64`, or `64..` for the \
                 rest of the space"
            ),
            Self::NoNodes => write!(f, "no [[node]] entries; a cluster needs at least one"),
            Self::DuplicateNode { name } => write!(f, "two nodes are named `{name}`"),
            Self::DuplicateAddr { addr } => write!(
                f,
                "two nodes both claim {addr}; one process holds one file, so one address \
                 cannot own two ranges"
            ),
            Self::MissingField { node, key } => write!(
                f,
                "node `{node}` has no `{key}`; a node either owns a range with `shards` or \
                 mirrors one with `replica`"
            ),
            Self::BothShardsAndReplica { node } => write!(
                f,
                "node `{node}` has both `shards` and `replica`; a replica holds exactly what \
                 its primary holds, so saying it twice is a way of saying it differently"
            ),
            Self::UnknownPrimary { node, primary } => {
                write!(
                    f,
                    "node `{node}` is a replica of `{primary}`, which is not one of the nodes"
                )
            }
            Self::ReplicaOfReplica { node, primary } => write!(
                f,
                "node `{node}` is a replica of `{primary}`, which is itself a replica; a replica \
                 mirrors the node a read goes to, and there is no chain"
            ),
            Self::TooFewForFailover { nodes } => write!(
                f,
                "this file has a replica and only {nodes} nodes; failing over needs a majority \
                 to agree, a majority of two is two, so a cluster of two can never use its \
                 copy. Add a third node - another range's primary counts - or drop the replica"
            ),
            Self::LeaderIsReplica { name } => write!(
                f,
                "schema_leader is `{name}`, which is a replica; the leader decides what a row \
                 key means rather than copying the decision, so it has to be a primary"
            ),
            Self::Overlap { a, b, from, to } => write!(
                f,
                "`{a}` and `{b}` both own shards {from}..{to}; each would answer half of every \
                 query over them and neither would say so"
            ),
            Self::Gap { from, to } => match to {
                Some(to) => write!(
                    f,
                    "shards {from}..{to} have no owner; a record id in that range could be \
                     written by a client and read back by nobody"
                ),
                None => write!(
                    f,
                    "shards {from}.. have no owner; the last node must end with `{from}..` so \
                     that every record id a client can choose belongs to somebody"
                ),
            },
            Self::NoLeader => write!(
                f,
                "no schema_leader; exactly one node has to own the row keys, or two nodes will \
                 give one string two row ids"
            ),
            Self::UnknownLeader { name } => {
                write!(f, "schema_leader is `{name}`, which is not one of the nodes")
            }
            Self::UnknownNode { name } => {
                write!(f, "--node is `{name}`, which is not one of the nodes")
            }
            Self::Unplaced { addr } => write!(
                f,
                "this daemon binds {addr}, which no node in the file claims; pass --node <name> \
                 to say which one it is"
            ),
        }
    }
}

impl core::error::Error for ConfigError {}

/// A parsed and validated file, before it knows which node is reading it.
///
/// Two steps because they fail for different reasons and at different times: the file can be
/// wrong on its own terms, or it can be right and not mention the daemon reading it.
#[derive(Clone, Debug)]
pub struct ClusterFile {
    /// Primaries first, in range order, then replicas.
    nodes: Vec<Node>,
    primary_count: usize,
    leader: usize,
    peer_ca_file: Option<String>,
}

/// One `[[node]]` block, before it is known whether the file as a whole makes sense.
///
/// A separate type from [`Node`] because a replica's range is not in the file: it comes from
/// the primary, and until every primary is known there is nothing to take it from.
struct Draft {
    name: String,
    addr: String,
    shards: Option<ShardRange>,
    replica: Option<String>,
}

impl ClusterFile {
    /// Parses and validates: ranges disjoint and total, one named leader, no repeated names.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut nodes: Vec<Vec<(String, String, usize)>> = Vec::new();
        let mut top: Vec<(String, String, usize)> = Vec::new();
        let mut in_node = false;

        for (i, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let n = i + 1;
            if line == "[[node]]" {
                nodes.push(Vec::new());
                in_node = true;
                continue;
            }
            if line.starts_with('[') {
                return Err(ConfigError::Syntax {
                    line: n,
                    why: "only [[node]] tables exist here",
                });
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::Syntax { line: n, why: "expected `key = \"value\"`" });
            };
            let key = key.trim().to_string();
            let value = unquote(value.trim())
                .ok_or(ConfigError::Syntax { line: n, why: "a value must be double-quoted" })?;
            if in_node {
                nodes.last_mut().expect("in_node implies one entry").push((key, value, n));
            } else {
                top.push((key, value, n));
            }
        }

        let mut leader_name = None;
        let mut peer_ca_file = None;
        for (key, value, line) in top {
            match key.as_str() {
                "schema_leader" => leader_name = Some(value),
                "peer_ca_file" => peer_ca_file = Some(value),
                _ => return Err(ConfigError::UnknownKey { line, key }),
            }
        }

        let mut parsed = Vec::with_capacity(nodes.len());
        for entry in nodes {
            let (mut name, mut addr, mut shards, mut replica) = (None, None, None, None);
            for (key, value, line) in entry {
                match key.as_str() {
                    "name" => name = Some(value),
                    "addr" => addr = Some(value),
                    "replica" => replica = Some(value),
                    "shards" => {
                        shards = Some(
                            ShardRange::parse(&value)
                                .ok_or(ConfigError::BadRange { line, text: value.clone() })?,
                        )
                    }
                    _ => return Err(ConfigError::UnknownKey { line, key }),
                }
            }
            // The name comes first so that the two errors below can use it. A node with no
            // name is reported by the field it is missing, which is the only handle there is.
            let name = name.ok_or(ConfigError::MissingField {
                node: addr.clone().unwrap_or_else(|| "(unnamed)".to_string()),
                key: "name",
            })?;
            let addr = addr.ok_or(ConfigError::MissingField { node: name.clone(), key: "addr" })?;
            if shards.is_some() && replica.is_some() {
                return Err(ConfigError::BothShardsAndReplica { node: name });
            }
            let shards = match (&shards, &replica) {
                (Some(_), _) => shards,
                (None, Some(_)) => None,
                // Neither: the message names `shards` because that is what a node written by
                // hand is usually missing, and it also names the alternative.
                (None, None) => {
                    return Err(ConfigError::MissingField { node: name, key: "shards" })
                }
            };
            parsed.push(Draft { name, addr, shards, replica });
        }

        Self::validated(parsed, leader_name, peer_ca_file)
    }

    fn validated(
        drafts: Vec<Draft>,
        leader_name: Option<String>,
        peer_ca_file: Option<String>,
    ) -> Result<Self, ConfigError> {
        if drafts.is_empty() {
            return Err(ConfigError::NoNodes);
        }
        for (i, n) in drafts.iter().enumerate() {
            if let Some(other) = drafts[..i].iter().find(|o| o.name == n.name) {
                return Err(ConfigError::DuplicateNode { name: other.name.clone() });
            }
            if let Some(other) = drafts[..i].iter().find(|o| o.addr == n.addr) {
                return Err(ConfigError::DuplicateAddr { addr: other.addr.clone() });
            }
        }

        // Primaries first, then their replicas, so that a replica's index is always above the
        // primary it mirrors and `owner` can search a contiguous prefix.
        let mut nodes: Vec<Node> = Vec::with_capacity(drafts.len());
        let mut primaries: Vec<&Draft> = drafts.iter().filter(|d| d.replica.is_none()).collect();
        // Sorted by where each range begins, so disjointness and totality are one walk. The
        // file's own order is not kept: it is the operator's, and it means nothing here.
        primaries.sort_by_key(|d| d.shards.expect("a primary has a range").start);
        for d in &primaries {
            nodes.push(Node {
                name: d.name.clone(),
                addr: d.addr.clone(),
                shards: d.shards.expect("a primary has a range"),
                replica_of: None,
            });
        }
        let primary_count = nodes.len();
        if primary_count == 0 {
            // Every node is a replica of something, so nothing owns anything. The named
            // primaries do not exist, which is what the next loop would have said one at a
            // time; saying it once is clearer.
            return Err(ConfigError::NoNodes);
        }
        for d in drafts.iter().filter(|d| d.replica.is_some()) {
            let of = d.replica.as_ref().expect("filtered");
            let at = nodes[..primary_count].iter().position(|n| n.name == *of);
            let at = match at {
                Some(at) => at,
                // Told apart so the message can say which mistake it was: naming nothing, or
                // naming a node that is itself a copy.
                None if drafts.iter().any(|o| o.name == *of) => {
                    return Err(ConfigError::ReplicaOfReplica {
                        node: d.name.clone(),
                        primary: of.clone(),
                    })
                }
                None => {
                    return Err(ConfigError::UnknownPrimary {
                        node: d.name.clone(),
                        primary: of.clone(),
                    })
                }
            };
            nodes.push(Node {
                name: d.name.clone(),
                addr: d.addr.clone(),
                shards: nodes[at].shards,
                replica_of: Some(at),
            });
        }

        if nodes[0].shards.start != 0 {
            return Err(ConfigError::Gap { from: 0, to: Some(nodes[0].shards.start) });
        }
        for pair in nodes[..primary_count].windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let Some(end) = a.shards.end else {
                // An open range that is not last swallows everything after it.
                return Err(ConfigError::Overlap {
                    a: a.name.clone(),
                    b: b.name.clone(),
                    from: b.shards.start,
                    to: b.shards.end.unwrap_or(ShardId::MAX),
                });
            };
            match b.shards.start.cmp(&end) {
                core::cmp::Ordering::Less => {
                    return Err(ConfigError::Overlap {
                        a: a.name.clone(),
                        b: b.name.clone(),
                        from: b.shards.start,
                        to: end.min(b.shards.end.unwrap_or(ShardId::MAX)),
                    })
                }
                core::cmp::Ordering::Greater => {
                    return Err(ConfigError::Gap { from: end, to: Some(b.shards.start) })
                }
                core::cmp::Ordering::Equal => {}
            }
        }
        if let Some(end) = nodes[..primary_count].last().expect("non-empty").shards.end {
            return Err(ConfigError::Gap { from: end, to: None });
        }

        if nodes.len() > primary_count && nodes.len() < 3 {
            return Err(ConfigError::TooFewForFailover { nodes: nodes.len() });
        }

        let leader_name = leader_name.ok_or(ConfigError::NoLeader)?;
        let leader = nodes
            .iter()
            .position(|n| n.name == leader_name)
            .ok_or_else(|| ConfigError::UnknownLeader { name: leader_name.clone() })?;
        if nodes[leader].replica_of.is_some() {
            return Err(ConfigError::LeaderIsReplica { name: leader_name });
        }

        Ok(Self { nodes, primary_count, leader, peer_ca_file })
    }

    /// Says which of these nodes is doing the reading.
    ///
    /// By name when the daemon was told one, otherwise by the address it was told to bind.
    /// The address is enough on its own because two nodes sharing one are already refused.
    pub fn for_node(self, name: Option<&str>, addr: &str) -> Result<ClusterConfig, ConfigError> {
        let this = match name {
            Some(name) => self
                .nodes
                .iter()
                .position(|n| n.name == name)
                .ok_or_else(|| ConfigError::UnknownNode { name: name.to_string() })?,
            None => self
                .nodes
                .iter()
                .position(|n| n.addr == addr)
                .ok_or_else(|| ConfigError::Unplaced { addr: addr.to_string() })?,
        };
        let groups = (0..self.primary_count)
            .map(|r| {
                let mut group = vec![r];
                group.extend(
                    self.nodes
                        .iter()
                        .enumerate()
                        .filter(|(_, n)| n.replica_of == Some(r))
                        .map(|(i, _)| i),
                );
                group
            })
            .collect();
        Ok(ClusterConfig {
            nodes: self.nodes,
            primary_count: self.primary_count,
            groups,
            leader: self.leader,
            this,
            peer_ca_file: self.peer_ca_file,
        })
    }

    /// The CA that signs a node's certificate, if the operator named one.
    ///
    /// The path, not the certificate. Loading one is a policy this crate does not own - the mode
    /// check that makes a key file a secret at all lives with the rest of it in `big-tls` - so
    /// what crosses this boundary is where to look.
    ///
    /// Shared by every node, unlike the certificate and key themselves: one file cannot name node
    /// A's private key without also naming node B's, so those are `big serve` flags. This is the
    /// half that is the same everywhere.
    pub fn peer_ca_file(&self) -> Option<&str> {
        self.peer_ca_file.as_deref()
    }

    /// Every node named in the file, before this daemon has worked out which one it is.
    ///
    /// The listener needs these before `for_node` runs: they are the names a client certificate
    /// is allowed to claim, and the certificate is checked during a handshake that happens long
    /// before any of the routing does.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
}

/// A validated file, read by a node that knows which one it is.
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// Primaries first, sorted by `shards.start`, disjoint and total. Replicas after them.
    nodes: Vec<Node>,
    /// How many of `nodes` are primaries, which is where the prefix ends.
    primary_count: usize,
    /// Every node holding each range, the configured primary first. Precomputed because it is
    /// asked for on the path of every write and never changes.
    groups: Vec<Vec<usize>>,
    leader: usize,
    this: usize,
    peer_ca_file: Option<String>,
}

impl ClusterConfig {
    /// The configuration of a database that is not clustered: one node, every shard, and
    /// nobody to disagree with.
    ///
    /// Not a special case - it is the general one with a peer count of zero, which is the
    /// point. If `big serve` without `--cluster` took a different path through the coordinator,
    /// that path would be the one nobody tests.
    pub fn solo(addr: &str) -> Self {
        Self {
            nodes: vec![Node {
                name: "local".to_string(),
                addr: addr.to_string(),
                shards: ShardRange { start: 0, end: None },
                replica_of: None,
            }],
            primary_count: 1,
            groups: vec![vec![0]],
            leader: 0,
            this: 0,
            peer_ca_file: None,
        }
    }

    /// A number every node holding the same cluster file computes alike.
    ///
    /// **This closes the one failure ownership-by-configuration could not see.** Two nodes given
    /// files that disagree - a range moved, a replica added, a different leader - used to be
    /// undetectable until somebody noticed a query answering half of itself. They cannot meet
    /// without exchanging this, and a mismatch is refused rather than served.
    ///
    /// Covers what every node has to agree on and nothing else: the names, the addresses, the
    /// ranges, which node copies which, and who leads the schema. Not which node *this* is, and
    /// not where its token file lives - those are local, and folding them in would make every
    /// node disagree with every other by construction.
    pub fn fingerprint(&self) -> u64 {
        // FNV-1a. What it is up against is a file somebody edited on one machine and not
        // another, not a file somebody forged: a node that could forge this could equally
        // forge the answer to any query.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let eat = |bytes: &[u8], h: &mut u64| {
            for b in bytes {
                *h ^= *b as u64;
                *h = h.wrapping_mul(0x100_0000_01b3);
            }
        };
        // Length first, so that two fields running together cannot be mistaken for one field
        // split differently.
        let text = |s: &str, h: &mut u64| {
            eat(&s.len().to_le_bytes(), h);
            eat(s.as_bytes(), h);
        };
        // In the order the nodes are held, which is derived - sorted by range, replicas after -
        // rather than the order the file happened to be written in.
        for node in &self.nodes {
            text(&node.name, &mut h);
            text(&node.addr, &mut h);
            text(&node.shards.to_string(), &mut h);
            eat(&node.replica_of.map_or(u64::MAX, |r| r as u64).to_le_bytes(), &mut h);
        }
        text(&self.nodes[self.leader].name.clone(), &mut h);
        h
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn this(&self) -> &Node {
        &self.nodes[self.this]
    }

    pub fn this_index(&self) -> usize {
        self.this
    }

    pub fn leader(&self) -> &Node {
        &self.nodes[self.leader]
    }

    pub fn leader_index(&self) -> usize {
        self.leader
    }

    /// Whether this node is the one that assigns row ids.
    pub fn leads_schema(&self) -> bool {
        self.leader == self.this
    }

    /// Whether this node holds every shard and nobody else holds a copy, which is what makes a
    /// fan-out unnecessary.
    pub fn owns_everything(&self) -> bool {
        self.nodes.len() == 1
    }

    /// Whether any node in this cluster is a copy of another.
    pub fn is_replicated(&self) -> bool {
        self.primary_count < self.nodes.len()
    }

    /// How many ranges there are, which is how many primaries the file named.
    ///
    /// A range is a *slot*, not a node: which node serves it can move, and moving it is what
    /// failing over means. Nothing else about a range moves - every node in its group already
    /// holds the data, which is why a promotion transfers no bytes.
    pub fn range_count(&self) -> usize {
        self.primary_count
    }

    /// Which range a shard falls in.
    pub fn range_of(&self, shard: ShardId) -> usize {
        self.nodes[..self.primary_count].partition_point(|n| n.shards.start <= shard) - 1
    }

    /// The range a node holds, whether it holds it as a primary or as a copy.
    pub fn range_of_node(&self, node: usize) -> Option<usize> {
        self.nodes.get(node).map(|n| n.replica_of.unwrap_or(node))
    }

    /// Every node holding one range, the configured primary first.
    pub fn group(&self, range: usize) -> &[usize] {
        &self.groups[range]
    }

    /// The map the cluster starts from: what the file says.
    ///
    /// **The file is a seed, not the truth.** It is what the map is before the agreement has
    /// decided anything, and every committed `Decision::Ranges` replaces it. That is the same
    /// rule ownership always followed - "the config's answer until the agreement has one" -
    /// widened from *who serves a range* to *what the ranges are*.
    ///
    /// Range ids are positions in the file here, and only here. After the first split they are
    /// minted from [`crate::raft::RangeMap::next_id`] and mean nothing positional.
    pub fn seed_map(&self) -> crate::raft::RangeMap {
        let ranges = (0..self.primary_count)
            .map(|r| crate::raft::Range {
                id: r as crate::raft::RangeId,
                shards: self.nodes[r].shards,
                group: self.groups[r].clone(),
                primary: r,
                moving: None,
            })
            .collect();
        // Nothing is behind before anything has happened, which is exactly what the file
        // asserts by naming a primary for every range.
        crate::raft::RangeMap { epoch: 0, ranges, stale: Vec::new(), schema_leader: self.leader }
    }

    /// The members the cluster starts from, in the file's order.
    pub fn seed_members(&self) -> Vec<crate::raft::Member> {
        self.nodes
            .iter()
            .map(|n| crate::raft::Member {
                name: n.name.clone(),
                addr: n.addr.clone(),
                state: crate::raft::MemberState::Voter,
            })
            .collect()
    }

    /// The nodes a read goes to, one per range, in range order.
    pub fn primaries(&self) -> impl Iterator<Item = usize> + '_ {
        0..self.primary_count
    }

    /// Every node holding a shard: the primary first, then its replicas in file order.
    ///
    /// A write goes to all of them and a read goes to the first. The order is the whole
    /// protocol: there is no election, no lease and no failure detector, so "which copy is
    /// authoritative" has to be a fact about the config file rather than about the moment.
    pub fn copies(&self, shard: ShardId) -> Vec<usize> {
        self.groups[self.range_of(shard)].clone()
    }

    /// The copies of one node's range, that node included.
    pub fn copies_of_node(&self, i: usize) -> Vec<usize> {
        self.groups[self.range_of_node(i).expect("every node holds a range")].clone()
    }

    /// Which node owns a shard.
    ///
    /// Total, so there is always one: the ranges were checked to cover the space before this
    /// type existed, which is what lets this return a node rather than an option.
    pub fn owner(&self, shard: ShardId) -> usize {
        // Every range starts at or below the one after it and the first starts at 0, so the
        // last range that begins at or below `shard` is the one containing it. Replicas sit
        // past `primary_count` and are not searched: a read has one destination.
        self.nodes[..self.primary_count].partition_point(|n| n.shards.start <= shard) - 1
    }

    /// Which node holds a record, which is the same lookup one shift earlier.
    pub fn owner_of_record(&self, record: RecordId) -> usize {
        self.owner(big_engine::shard_of(record))
    }

    pub fn peer_ca_file(&self) -> Option<&str> {
        self.peer_ca_file.as_deref()
    }
}

/// Everything from the first `#` outside a quoted string.
///
/// Inside a string it is a character like any other: a token file path may legitimately
/// contain one, and truncating there would leave a path that resolves to something else.
fn strip_comment(line: &str) -> &str {
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

/// The contents of a double-quoted value, or `None` if it is not one.
///
/// No escapes. A cluster file holds names, addresses and a path; a value that needs an escape
/// is a value this format should refuse rather than half-understand.
fn unquote(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then(|| inner.to_string())
}
