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

//! The agreement's own messages.
//!
//! Separate from the rest because they are the only messages that are not a question with an
//! answer: a reply is a new request in the other direction, so nothing here pairs a request
//! type with a response body.

use super::*;
use crate::raft;

/// One Raft message, on its way to `POST /internal/raft`.
///
/// One-way: a reply is a new request in the other direction, not the body of this one's
/// response. That is what a consensus protocol expects - a node answers a vote when it has
/// decided, not when the socket is still open - and it keeps a slow peer from holding a
/// worker on the node that is waiting for it.
pub fn put_raft(out: &mut Vec<u8>, m: &raft::Message) {
    match m {
        raft::Message::RequestVote { term, candidate, last_index, last_term } => {
            put_u8(out, 0);
            put_u64(out, *term);
            put_u64(out, *candidate as u64);
            put_u64(out, *last_index);
            put_u64(out, *last_term);
        }
        raft::Message::VoteReply { term, from, granted } => {
            put_u8(out, 1);
            put_u64(out, *term);
            put_u64(out, *from as u64);
            put_bool(out, *granted);
        }
        raft::Message::Append { term, leader, prev_index, prev_term, entries, commit } => {
            put_u8(out, 2);
            put_u64(out, *term);
            put_u64(out, *leader as u64);
            put_u64(out, *prev_index);
            put_u64(out, *prev_term);
            put_u64(out, *commit);
            put_count(out, entries.len());
            for entry in entries {
                put_u64(out, entry.term);
                match &entry.decision {
                    raft::Decision::Noop => put_u8(out, 0),
                    raft::Decision::Ranges(m) => {
                        put_u8(out, 1);
                        put_range_map(out, m);
                    }
                    raft::Decision::Members(ms) => {
                        put_u8(out, 2);
                        put_members(out, ms);
                    }
                }
            }
        }
        raft::Message::Snapshot { term, leader, index, last_term, ranges, members } => {
            put_u8(out, 4);
            put_u64(out, *term);
            put_u64(out, *leader as u64);
            put_u64(out, *index);
            put_u64(out, *last_term);
            put_range_map(out, ranges);
            put_members(out, members);
        }
        raft::Message::AppendReply { term, from, success, match_index } => {
            put_u8(out, 3);
            put_u64(out, *term);
            put_u64(out, *from as u64);
            put_bool(out, *success);
            put_u64(out, *match_index);
        }
    }
}

pub fn get_raft(bytes: &[u8]) -> Result<raft::Message> {
    let mut r = Reader::new(bytes);
    let node = |v: u64| v as usize;
    let out = match r.u8()? {
        0 => raft::Message::RequestVote {
            term: r.u64()?,
            candidate: node(r.u64()?),
            last_index: r.u64()?,
            last_term: r.u64()?,
        },
        1 => raft::Message::VoteReply { term: r.u64()?, from: node(r.u64()?), granted: r.bool()? },
        2 => {
            let term = r.u64()?;
            let leader = node(r.u64()?);
            let prev_index = r.u64()?;
            let prev_term = r.u64()?;
            let commit = r.u64()?;
            let n = r.count()?;
            let mut entries = reserve(n);
            for _ in 0..n {
                let term = r.u64()?;
                let decision = match r.u8()? {
                    0 => raft::Decision::Noop,
                    1 => raft::Decision::Ranges(get_range_map(&mut r)?),
                    2 => raft::Decision::Members(get_members(&mut r)?),
                    tag => return Err(WireError::BadTag { what: "decision", tag }),
                };
                entries.push(raft::Entry { term, decision });
            }
            raft::Message::Append { term, leader, prev_index, prev_term, entries, commit }
        }
        3 => raft::Message::AppendReply {
            term: r.u64()?,
            from: node(r.u64()?),
            success: r.bool()?,
            match_index: r.u64()?,
        },
        4 => raft::Message::Snapshot {
            term: r.u64()?,
            leader: node(r.u64()?),
            index: r.u64()?,
            last_term: r.u64()?,
            ranges: get_range_map(&mut r)?,
            members: get_members(&mut r)?,
        },
        tag => return Err(WireError::BadTag { what: "agreement message", tag }),
    };
    finished(&r)?;
    Ok(out)
}

pub fn encode_raft(m: &raft::Message) -> Vec<u8> {
    let mut out = Vec::new();
    put_raft(&mut out, m);
    out
}

// -----------------------------------------------------------------------------------------
// The two values the agreement carries
//
// Both are small - a map is one line per range, a member list one line per node - and both
// travel inside an append, so they are encoded here rather than as messages of their own.
// -----------------------------------------------------------------------------------------

fn put_range_map(out: &mut Vec<u8>, m: &raft::RangeMap) {
    put_u64(out, m.epoch);
    put_u64(out, m.schema_leader as u64);
    put_count(out, m.ranges.len());
    for r in &m.ranges {
        put_u64(out, r.id);
        put_u64(out, r.shards.start);
        put_opt_u64(out, r.shards.end);
        put_u64(out, r.primary as u64);
        put_count(out, r.group.len());
        for n in &r.group {
            put_u64(out, *n as u64);
        }
        match &r.moving {
            None => put_u8(out, 0),
            Some(mv) => {
                put_u8(
                    out,
                    match mv.state {
                        raft::MoveState::Seeding => 1,
                        raft::MoveState::Cutover => 2,
                    },
                );
                put_u64(out, mv.target as u64);
            }
        }
    }
    put_count(out, m.stale.len());
    for n in &m.stale {
        put_u64(out, *n as u64);
    }
    put_count(out, m.reserved.len());
    for (table, upto) in &m.reserved {
        put_str(out, table);
        put_u64(out, *upto);
    }
    put_bool(out, m.schema_ready);
}

fn get_range_map(r: &mut Reader<'_>) -> Result<raft::RangeMap> {
    let epoch = r.u64()?;
    let schema_leader = r.u64()? as raft::NodeId;
    let n = r.count()?;
    let mut ranges = reserve(n);
    for _ in 0..n {
        let id = r.u64()?;
        let start = r.u64()?;
        let end = r.opt_u64()?;
        let primary = r.u64()? as raft::NodeId;
        let count = r.count()?;
        let mut group = reserve(count);
        for _ in 0..count {
            group.push(r.u64()? as raft::NodeId);
        }
        let moving = match r.u8()? {
            0 => None,
            1 => Some(raft::Move {
                target: r.u64()? as raft::NodeId,
                state: raft::MoveState::Seeding,
            }),
            2 => Some(raft::Move {
                target: r.u64()? as raft::NodeId,
                state: raft::MoveState::Cutover,
            }),
            tag => return Err(WireError::BadTag { what: "move state", tag }),
        };
        ranges.push(raft::Range {
            id,
            shards: big_engine::ShardRange { start, end },
            group,
            primary,
            moving,
        });
    }
    let count = r.count()?;
    let mut stale = reserve(count);
    for _ in 0..count {
        stale.push(r.u64()? as raft::NodeId);
    }
    let count = r.count()?;
    let mut reserved = reserve(count);
    for _ in 0..count {
        reserved.push((r.str()?, r.u64()?));
    }
    let schema_ready = r.bool()?;
    Ok(raft::RangeMap { epoch, ranges, stale, schema_leader, reserved, schema_ready })
}

fn put_members(out: &mut Vec<u8>, members: &[raft::Member]) {
    put_count(out, members.len());
    for m in members {
        put_str(out, &m.name);
        put_str(out, &m.addr);
        put_u8(
            out,
            match m.state {
                raft::MemberState::Learner => 0,
                raft::MemberState::Voter => 1,
                raft::MemberState::Draining => 2,
                raft::MemberState::Gone => 3,
            },
        );
    }
}

fn get_members(r: &mut Reader<'_>) -> Result<Vec<raft::Member>> {
    let n = r.count()?;
    let mut out = reserve(n);
    for _ in 0..n {
        let name = r.str()?;
        let addr = r.str()?;
        let state = match r.u8()? {
            0 => raft::MemberState::Learner,
            1 => raft::MemberState::Voter,
            2 => raft::MemberState::Draining,
            3 => raft::MemberState::Gone,
            tag => return Err(WireError::BadTag { what: "member state", tag }),
        };
        out.push(raft::Member { name, addr, state });
    }
    Ok(out)
}
