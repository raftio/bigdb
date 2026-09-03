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

//! Containers, row sets, matches and values.
//!
//! The bottom of the encoding: everything a plan's *answer* is made of. A container is copied
//! rather than re-encoded - see the module doc one level up - which makes this the one place
//! where the wire format and the page format are deliberately the same bytes.

use super::*;

// ---------------------------------------------------------------------------------------
// Containers, row sets, matches
// ---------------------------------------------------------------------------------------

/// The tags are the leaf cell's `ContainerType` numbering, minus the two that only mean
/// something on a page: a bitmap living in its own page and a bitmap with a delta are the same
/// set of bits once resolved, and which one it was is a storage decision this side of the wire
/// has no business inheriting.
mod container_tag {
    pub const ARRAY: u8 = 0;
    pub const RUN: u8 = 1;
    pub const BITMAP: u8 = 2;
}

pub fn put_container(out: &mut Vec<u8>, c: ContainerRef<'_>) {
    match c {
        ContainerRef::Array(a) => {
            put_u8(out, container_tag::ARRAY);
            put_count(out, a.len());
            out.extend_from_slice(bytemuck::cast_slice(a));
        }
        ContainerRef::Run(r) => {
            put_u8(out, container_tag::RUN);
            put_count(out, r.len());
            out.extend_from_slice(bytemuck::cast_slice(r));
        }
        ContainerRef::Bitmap(b) => {
            put_u8(out, container_tag::BITMAP);
            out.extend_from_slice(bytemuck::cast_slice(&b[..]));
        }
    }
}

pub fn get_container(r: &mut Reader<'_>) -> Result<Container> {
    let tag = r.u8()?;
    let c = match tag {
        container_tag::ARRAY => {
            let n = r.count()?;
            let bytes = r.take(n.checked_mul(2).ok_or(WireError::Truncated)?)?;
            Container::Array(
                bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect(),
            )
        }
        container_tag::RUN => {
            let n = r.count()?;
            let bytes = r.take(n.checked_mul(4).ok_or(WireError::Truncated)?)?;
            Container::Run(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| Interval {
                        start: u16::from_le_bytes([c[0], c[1]]),
                        last: u16::from_le_bytes([c[2], c[3]]),
                    })
                    .collect(),
            )
        }
        container_tag::BITMAP => {
            let bytes = r.take(big_container::BITMAP_BYTES)?;
            let mut words = Box::new([0u64; big_container::BITMAP_WORDS]);
            for (w, c) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
                *w = u64::from_le_bytes(*c);
            }
            Container::Bitmap(words)
        }
        tag => return Err(WireError::BadTag { what: "container type", tag }),
    };
    // Structurally fine and still nonsense: an array whose values descend, a run whose ends
    // are the wrong way round. The set algebra assumes neither can happen, so the check is
    // here rather than in the operators that would quietly give a wrong answer.
    if !c.as_ref().is_well_formed() {
        return Err(WireError::Malformed("a container's values are not in order"));
    }
    Ok(c)
}

pub fn put_rowset(out: &mut Vec<u8>, rows: &RowSet) {
    put_count(out, rows.len());
    for (slot, c) in rows.iter() {
        put_u64(out, slot);
        put_container(out, c);
    }
}

pub fn get_rowset(r: &mut Reader<'_>) -> Result<RowSet> {
    let n = r.count()?;
    let mut out = RowSet::new();
    for _ in 0..n {
        let slot = r.u64()?;
        out.insert(slot, get_container(r)?);
    }
    Ok(out)
}

/// The answer to a predicate, shard by shard.
///
/// Owners contribute disjoint shard sets, so the coordinator's merge is `Matches::or` and
/// cannot double-count. That property is a consequence of the ownership check at startup, not
/// of anything in this encoding.
pub fn put_matches(out: &mut Vec<u8>, m: &Matches) {
    let shards: Vec<ShardId> = m.shards().collect();
    put_count(out, shards.len());
    for shard in shards {
        put_u64(out, shard);
        put_rowset(out, m.get(shard).expect("shards() lists what get() finds"));
    }
}

pub fn get_matches(r: &mut Reader<'_>) -> Result<Matches> {
    let n = r.count()?;
    let mut out = Matches::new();
    for _ in 0..n {
        let shard = r.u64()?;
        out.insert(shard, get_rowset(r)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------

mod value_tag {
    pub const ROWS: u8 = 0;
    pub const COUNT: u8 = 1;
    pub const SUM: u8 = 2;
    pub const SIGNED_SUM: u8 = 3;
    pub const EXTREME: u8 = 4;
    pub const SIGNED_EXTREME: u8 = 5;
    pub const GROUPS: u8 = 6;
    pub const TABLE: u8 = 7;
    pub const PAIRS: u8 = 8;
    pub const REAL_SUM: u8 = 9;
    pub const REAL_EXTREME: u8 = 10;
}

pub fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Rows(m) => {
            put_u8(out, value_tag::ROWS);
            put_matches(out, m);
        }
        Value::Count(n) => {
            put_u8(out, value_tag::COUNT);
            put_u64(out, *n);
        }
        Value::Sum(n) => {
            put_u8(out, value_tag::SUM);
            put_u128(out, *n);
        }
        Value::SignedSum(n) => {
            put_u8(out, value_tag::SIGNED_SUM);
            put_i128(out, *n);
        }
        Value::Extreme(x) => {
            put_u8(out, value_tag::EXTREME);
            put_opt_u64(out, *x);
        }
        Value::SignedExtreme(x) => {
            put_u8(out, value_tag::SIGNED_EXTREME);
            put_opt_i64(out, *x);
        }
        // Sent as bits rather than as a decimal spelling, so a total does not change on its way
        // between two nodes. The fold that produced it is not associative; the wire must not add
        // a second reason for two runs to disagree.
        Value::RealSum(n) => {
            put_u8(out, value_tag::REAL_SUM);
            put_u64(out, n.to_bits());
        }
        Value::RealExtreme(x) => {
            put_u8(out, value_tag::REAL_EXTREME);
            put_opt_u64(out, x.map(f64::to_bits));
        }
        Value::Groups(g) => {
            put_u8(out, value_tag::GROUPS);
            put_count(out, g.len());
            for group in g {
                put_group(out, group);
            }
        }
        Value::Pairs(pairs) => {
            put_u8(out, value_tag::PAIRS);
            put_count(out, pairs.len());
            for p in pairs {
                put_group(out, &p.left);
                put_group(out, &p.right);
            }
        }
        Value::Table(rows) => {
            put_u8(out, value_tag::TABLE);
            put_count(out, rows.len());
            for row in rows {
                put_u64(out, row.record);
                put_count(out, row.values.len());
                for v in &row.values {
                    put_projection(out, v);
                }
            }
        }
    }
}

/// Which shape one projected cell is. A projection over a table that stores its values can
/// answer with a key or a list of them, so the tag has to say which rather than being implied
/// by the plan - a coordinator merges bodies from several nodes and reads them before it has
/// resolved anything against a schema.
mod projection_tag {
    pub const ABSENT: u8 = 0;
    pub const INT: u8 = 1;
    pub const TEXT: u8 = 2;
    pub const TEXTS: u8 = 3;
    pub const REAL: u8 = 4;
}

fn put_projection(out: &mut Vec<u8>, p: &Projection) {
    match p {
        Projection::Absent => put_u8(out, projection_tag::ABSENT),
        Projection::Int(v) => {
            put_u8(out, projection_tag::INT);
            put_i128(out, *v);
        }
        // Bits, so a projected value is the same number on both sides of the wire.
        Projection::Real(v) => {
            put_u8(out, projection_tag::REAL);
            put_u64(out, v.to_bits());
        }
        Projection::Text(s) => {
            put_u8(out, projection_tag::TEXT);
            put_str(out, s);
        }
        Projection::Texts(v) => {
            put_u8(out, projection_tag::TEXTS);
            put_count(out, v.len());
            for s in v {
                put_str(out, s);
            }
        }
    }
}

fn get_projection(r: &mut Reader<'_>) -> Result<Projection> {
    Ok(match r.u8()? {
        projection_tag::ABSENT => Projection::Absent,
        projection_tag::INT => Projection::Int(r.i128()?),
        projection_tag::REAL => Projection::Real(f64::from_bits(r.u64()?)),
        projection_tag::TEXT => Projection::Text(r.str()?),
        projection_tag::TEXTS => {
            let n = r.count()?;
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(r.str()?);
            }
            Projection::Texts(out)
        }
        tag => return Err(WireError::BadTag { what: "projected cell", tag }),
    })
}

/// How a group says which one it is.
///
/// Tagged rather than written as a bare number, because the two kinds of identity are not
/// interchangeable: a row id means nothing without the dictionary that issued it, and a bucket
/// means the same thing on every node without one. A reader that took either as the other would
/// fold two different groups together and report a number nobody could trace.
mod group_tag {
    pub const ROW: u8 = 0;
    pub const BUCKET: u8 = 1;
}

mod unit_tag {
    pub const DAYS: u8 = 0;
    pub const SECONDS: u8 = 1;
}

fn put_group(out: &mut Vec<u8>, g: &Group) {
    match g.at {
        GroupAt::Row(row) => {
            out.push(group_tag::ROW);
            put_u64(out, row);
        }
        GroupAt::Bucket { start, unit } => {
            out.push(group_tag::BUCKET);
            put_u64(out, start as u64);
            out.push(match unit {
                TimeUnit::Days => unit_tag::DAYS,
                TimeUnit::Seconds => unit_tag::SECONDS,
            });
        }
    }
    put_opt_str(out, g.key.as_deref());
    put_value(out, &g.value);
}

fn get_group(r: &mut Reader<'_>, depth: usize) -> Result<Group> {
    let at = match r.u8()? {
        group_tag::ROW => GroupAt::Row(r.u64()?),
        group_tag::BUCKET => {
            let start = r.u64()? as i64;
            let unit = match r.u8()? {
                unit_tag::DAYS => TimeUnit::Days,
                unit_tag::SECONDS => TimeUnit::Seconds,
                tag => return Err(WireError::BadTag { what: "a bucket's unit", tag }),
            };
            GroupAt::Bucket { start, unit }
        }
        tag => return Err(WireError::BadTag { what: "a group's identity", tag }),
    };
    Ok(Group { at, key: r.opt_str()?, value: Box::new(get_value_at(r, depth + 1)?) })
}

pub fn get_value(r: &mut Reader<'_>) -> Result<Value> {
    get_value_at(r, 0)
}

fn get_value_at(r: &mut Reader<'_>, depth: usize) -> Result<Value> {
    if depth >= MAX_DEPTH {
        return Err(WireError::TooDeep);
    }
    let tag = r.u8()?;
    Ok(match tag {
        value_tag::ROWS => Value::Rows(get_matches(r)?),
        value_tag::COUNT => Value::Count(r.u64()?),
        value_tag::SUM => Value::Sum(r.u128()?),
        value_tag::SIGNED_SUM => Value::SignedSum(r.i128()?),
        value_tag::EXTREME => Value::Extreme(r.opt_u64()?),
        value_tag::SIGNED_EXTREME => Value::SignedExtreme(r.opt_i64()?),
        value_tag::REAL_SUM => Value::RealSum(f64::from_bits(r.u64()?)),
        value_tag::REAL_EXTREME => Value::RealExtreme(r.opt_u64()?.map(f64::from_bits)),
        value_tag::GROUPS => {
            let n = r.count()?;
            let mut groups = Vec::with_capacity(n);
            for _ in 0..n {
                groups.push(get_group(r, depth)?);
            }
            Value::Groups(groups)
        }
        value_tag::PAIRS => {
            let n = r.count()?;
            let mut pairs = Vec::with_capacity(n);
            for _ in 0..n {
                pairs.push(Pair { left: get_group(r, depth)?, right: get_group(r, depth)? });
            }
            Value::Pairs(pairs)
        }
        value_tag::TABLE => {
            let n = r.count()?;
            let mut rows = Vec::with_capacity(n);
            for _ in 0..n {
                let record = r.u64()?;
                let cells = r.count()?;
                let mut values = Vec::with_capacity(cells);
                for _ in 0..cells {
                    values.push(get_projection(r)?);
                }
                rows.push(Projected { record, values });
            }
            Value::Table(rows)
        }
        tag => return Err(WireError::BadTag { what: "value", tag }),
    })
}

pub fn encode_value(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    put_value(&mut out, v);
    out
}

pub fn decode_value(bytes: &[u8]) -> Result<Value> {
    let mut r = Reader::new(bytes);
    let v = get_value(&mut r)?;
    finished(&r)?;
    Ok(v)
}
