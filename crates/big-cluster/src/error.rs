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

//! What can go wrong once there is more than one machine, and what each one is worth.
//!
//! Every variant here carries the node and, where it is the point, the shard range: an
//! operator reading a `503` needs to know which part of the space stopped answering, and a
//! count that is missing a node's contribution looks exactly like a correct count.
//!
//! The status and the code live on the error rather than in a table in the HTTP layer, because
//! the classification is a property of the failure. `big-http` maps the engine's own errors
//! because it has to see the whole tree; it does not have to see this one.

use crate::config::ConfigError;
use crate::wire::WireError;
use big_embed::ApiError;

/// A failure of the cluster, or of the node under it.
#[derive(Debug)]
pub enum ClusterError {
    /// The local engine refused. Classified by `big-http` exactly as it always was: a
    /// coordinator that is also an owner is still an owner.
    Local(ApiError),
    /// The cluster file was refused. Only reachable at startup.
    Config(ConfigError),
    /// An owner could not be reached at all. **The whole query fails.**
    ///
    /// No partial answer, no `partial: true` beside it. A count missing a node's contribution
    /// is indistinguishable from a correct one, and there is no downstream check that would
    /// catch it.
    Unreachable { node: String, shards: String, why: String },
    /// The schema leader could not be reached, on a request that needed it.
    ///
    /// Reads never produce this, and neither does a write whose keys this node already knows.
    /// A write introducing a new key does, and is refused rather than queued: assigning the id
    /// locally and reconciling later is how one string ends up with two row ids.
    LeaderUnreachable { node: String, why: String },
    /// A peer was reached and refused. Its status and code travel out unchanged, because the
    /// peer is the one that knows why.
    Peer { node: String, status: u16, code: String, message: String },
    /// A peer answered with bytes this build cannot read.
    Wire { node: String, why: WireError },
    /// A peer answered the wrong shape of answer - a count where rows were asked for.
    /// Structurally readable, semantically impossible, and never worth merging.
    Mismatch { node: String, what: &'static str },
    /// Two totals that do not fit in one. Refused rather than wrapped: a sum that silently
    /// wrapped is the one kind of wrong answer nothing downstream can question.
    Overflow,
    /// The request outlived its budget.
    Timeout,
    /// This node holds the range and may not answer for it.
    ///
    /// It has lost touch with the agreement, so it has to assume it may already have been
    /// replaced. Two nodes serving one range would each answer half of every query and neither
    /// would say so, which is the one failure a client could never see - so a node that cannot
    /// prove it is still the primary stops being one.
    NotServing { node: String, shards: String },
    /// An operator asked for something the cluster will not do, and the message says why.
    ///
    /// A string rather than a variant per cause: these are read by a person at a terminal, they
    /// are not branched on, and every one of them names what to do instead.
    Refused(String),
    /// **The map moved underneath a routed request.** The coordinator sent these records to
    /// whoever owned them a decision ago; this node does not own them now.
    ///
    /// Retryable, and the retry is the point: the coordinator learns the map it was missing
    /// and sends the batch to the node that does own them. Without it, a write during a split
    /// or a move lands on a node no read will ever ask - a loss with nothing to report it.
    StaleRoute { node: String, wanted: String, epoch: u64, mine: u64 },
    /// **The one window a move denies anything.** Writes to the range being handed over are
    /// refused while the last difference is copied, so that the copy is made against something
    /// that is not moving.
    ///
    /// Retryable, and one range wide: everything else the cluster holds is untouched, and
    /// reads of this range are still answered by the node that has not let go of it yet.
    RangeMoving { node: String, shards: String },
    /// **Half applied.** Some nodes took it and some did not.
    ///
    /// There is no transaction across nodes and this is what that costs. Stated as plainly as
    /// the absence of a WAL is stated in `architecture.md`, and for the same reason: an
    /// atomicity guarantee that exists in the documentation and not in the code is worse than
    /// none, because callers build on it. A caller who needs all-or-nothing sends a batch whose
    /// records fall inside one owner's range, which is a property they can compute themselves
    /// from `SHARD_WIDTH`.
    ///
    /// `what` names the thing that is half applied, because the two cases read differently to
    /// an operator: a batch of facts can be sent again, and a schema change that landed on
    /// three nodes out of four has to be finished by hand.
    Partial { what: &'static str, committed: Vec<String>, failed: Vec<String> },
}

impl ClusterError {
    /// The HTTP status this failure is worth.
    ///
    /// [`Self::Local`] answers `500` here and never reaches it in practice: `big-http` unwraps
    /// it and classifies the engine error itself, which is the only place that can.
    pub fn status(&self) -> u16 {
        match self {
            Self::Local(_) | Self::Config(_) | Self::Overflow | Self::Partial { .. } => 500,
            // The request was understood and will not be done. 409 rather than 400: nothing
            // about it is malformed, and it may well succeed once the cluster is in another
            // state.
            Self::Refused(_) => 409,
            Self::Unreachable { .. }
            | Self::LeaderUnreachable { .. }
            | Self::NotServing { .. }
            | Self::StaleRoute { .. }
            | Self::RangeMoving { .. } => 503,
            Self::Peer { status, .. } => *status,
            Self::Wire { .. } | Self::Mismatch { .. } => 502,
            Self::Timeout => 504,
        }
    }

    /// Whether this failure is a map that moved underneath a routed request, which is the one
    /// failure worth trying again unchanged.
    pub fn is_stale_route(&self) -> bool {
        match self {
            Self::StaleRoute { .. } | Self::RangeMoving { .. } => true,
            // A refusal travels out of a peer as a status and a code, so the coordinator sees
            // the peer's verdict rather than the typed error the peer built.
            Self::Peer { code, .. } => code == "stale_route" || code == "range_moving",
            Self::Partial { failed, .. } => {
                failed.iter().any(|f| f.contains("stale_route") || f.contains("range_moving"))
            }
            _ => false,
        }
    }

    /// Whether this failure is "that node did not answer" rather than "that node said no".
    ///
    /// The distinction is what makes a fallback safe. A copy that could not be reached is
    /// worth asking another copy about; a copy that *refused* has given an answer, and asking
    /// somebody else the same refused question turns a clear failure into a confusing one.
    /// A node that has stood down counts as unreachable, because that is exactly what it is
    /// asking to be treated as.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            Self::Unreachable { .. }
                | Self::NotServing { .. }
                | Self::StaleRoute { .. }
                | Self::RangeMoving { .. }
                | Self::Timeout
        )
    }

    /// The stable, machine-readable half. A client that matches on prose breaks when the prose
    /// is improved.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Local(_) => "internal",
            Self::Config(_) => "cluster_misconfigured",
            Self::Unreachable { .. } => "owner_unreachable",
            Self::LeaderUnreachable { .. } => "schema_leader_unreachable",
            Self::Peer { .. } => "peer_refused",
            Self::Wire { .. } => "peer_unreadable",
            Self::Mismatch { .. } => "peer_mismatch",
            Self::Overflow => "sum_overflow",
            Self::Timeout => "query_timeout",
            Self::NotServing { .. } => "not_serving",
            Self::Refused(_) => "refused",
            Self::StaleRoute { .. } => "stale_route",
            Self::RangeMoving { .. } => "range_moving",
            Self::Partial { .. } => "partially_applied",
        }
    }
}

impl core::fmt::Display for ClusterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Local(e) => write!(f, "{e}"),
            Self::Config(e) => write!(f, "the cluster file is not usable: {e}"),
            // What happened, and nothing about what it means. This message is read on a
            // failed query, on a half-applied batch and in a digest report, and the sentence
            // that belongs to one of those reads as noise in the other two - the policy lives
            // in the stable code and in docs/clustering.md, which is where it stays true.
            Self::Unreachable { node, shards, why } => {
                write!(f, "`{node}` holds shards {shards} and could not be reached ({why})")
            }
            Self::LeaderUnreachable { node, why } => write!(
                f,
                "the schema leader `{node}` could not be reached ({why}); a key it has never \
                 assigned a row id cannot be written until it is back"
            ),
            Self::Peer { node, status, code, message } => {
                write!(f, "`{node}` refused with {status} {code}: {message}")
            }
            Self::Wire { node, why } => write!(f, "`{node}` sent something unreadable: {why}"),
            Self::Mismatch { node, what } => {
                write!(f, "`{node}` answered with {what}, which does not belong to this query")
            }
            Self::Overflow => write!(f, "the totals from two nodes do not fit in one number"),
            Self::Timeout => write!(f, "the request ran out of time"),
            Self::NotServing { node, shards } => write!(
                f,
                "`{node}` holds shards {shards} and has lost touch with the agreement, so it \
                 has stopped answering for them rather than risk a second node answering too"
            ),
            Self::Refused(why) => write!(f, "{why}"),
            Self::RangeMoving { node, shards } => write!(
                f,
                "`{node}` is handing shards {shards} to another node and is not taking writes \
                 for them while the last of the copy is made. Send it again in a moment - \
                 reads of these shards are still being answered, and nothing else is affected"
            ),
            Self::StaleRoute { node, wanted, epoch, mine } => write!(
                f,
                "`{node}` was sent records for shards {wanted} under map epoch {epoch}, and its \
                 own map is at {mine}; the range moved. Send it again - a coordinator that \
                 refreshes its map routes to whoever holds them now"
            ),
            Self::Partial { what, committed, failed } => write!(
                f,
                "{what} is half applied: it landed on {}, and was refused on {}. There is no \
                 transaction across nodes; a batch that has to be all or nothing is one whose \
                 records fall in a single owner's range",
                committed.join(", "),
                failed.join("; ")
            ),
        }
    }
}

impl core::error::Error for ClusterError {}

impl From<ApiError> for ClusterError {
    fn from(e: ApiError) -> Self {
        Self::Local(e)
    }
}

impl From<ConfigError> for ClusterError {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

pub type Result<T> = core::result::Result<T, ClusterError>;
