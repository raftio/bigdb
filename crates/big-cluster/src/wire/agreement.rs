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
                    raft::Decision::Own(o) => {
                        put_u8(out, 1);
                        put_count(out, o.primary.len());
                        for p in &o.primary {
                            put_u64(out, *p as u64);
                        }
                        put_count(out, o.stale.len());
                        for n in &o.stale {
                            put_u64(out, *n as u64);
                        }
                    }
                }
            }
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
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                let term = r.u64()?;
                let decision = match r.u8()? {
                    0 => raft::Decision::Noop,
                    1 => {
                        let list = |r: &mut Reader<'_>| -> Result<Vec<raft::NodeId>> {
                            let count = r.count()?;
                            let mut out = Vec::with_capacity(count);
                            for _ in 0..count {
                                out.push(node(r.u64()?));
                            }
                            Ok(out)
                        };
                        let primary = list(&mut r)?;
                        let stale = list(&mut r)?;
                        raft::Decision::Own(raft::Ownership { primary, stale })
                    }
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
