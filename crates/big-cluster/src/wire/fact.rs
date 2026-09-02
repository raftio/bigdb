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

//! Facts, key assignments and schema changes - what a write says.
//!
//! Everything here owns its strings. A batch arrives borrowed from one request body and leaves
//! split by owner across several, so the pieces outlive what they were parsed out of.
//!
//! No field ids cross this boundary. They are each node's own numbering; names do the
//! resolving, and row ids are the only ids with a protocol because they are the only ones that
//! have to mean the same thing everywhere.

use super::*;

/// One fact, owning its strings.
///
/// [`big_embed::Fact`] borrows, which is right for the call it was written for and wrong for a
/// batch that is being split by owner and sent to several places: the split outlives whatever
/// the request body was parsed out of.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OwnedFact {
    pub field: String,
    pub record: RecordId,
    pub value: FactValue,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FactValue {
    Int(u64),
    Signed(i64),
    /// `f64::to_bits` of a float value, so a fact crosses the wire bit-exact rather than
    /// through a decimal spelling that would have to round-trip.
    Float(u64),
    Key(String),
    Bool(bool),
    /// A key with the moment it happened, for a time quantum field.
    Time {
        value: String,
        unix_seconds: i64,
    },
}

impl OwnedFact {
    /// The owned form, for a caller holding facts that borrow something about to go away.
    ///
    /// The inverse of [`OwnedFact::as_fact`], and the slow direction on purpose: a batch is
    /// parsed into borrowed facts because that costs nothing, and only a batch that has to be
    /// *shipped to a peer* pays to own them.
    pub fn from_fact(fact: &big_embed::Fact<'_>) -> Self {
        let (field, record, value) = match fact {
            big_embed::Fact::Int { field, record, value } => {
                (*field, *record, FactValue::Int(*value))
            }
            big_embed::Fact::Signed { field, record, value } => {
                (*field, *record, FactValue::Signed(*value))
            }
            big_embed::Fact::Float { field, record, bits } => {
                (*field, *record, FactValue::Float(*bits))
            }
            big_embed::Fact::Bool { field, record, value } => {
                (*field, *record, FactValue::Bool(*value))
            }
            big_embed::Fact::Key { field, record, value } => {
                (*field, *record, FactValue::Key((*value).to_string()))
            }
            big_embed::Fact::Time { field, record, value, unix_seconds } => (
                *field,
                *record,
                FactValue::Time { value: (*value).to_string(), unix_seconds: *unix_seconds },
            ),
        };
        Self { field: field.to_string(), record, value }
    }

    /// The borrowed form the engine takes, valid as long as this one is.
    pub fn as_fact(&self) -> big_embed::Fact<'_> {
        match &self.value {
            FactValue::Int(v) => {
                big_embed::Fact::Int { field: &self.field, record: self.record, value: *v }
            }
            FactValue::Signed(v) => {
                big_embed::Fact::Signed { field: &self.field, record: self.record, value: *v }
            }
            FactValue::Float(v) => {
                big_embed::Fact::Float { field: &self.field, record: self.record, bits: *v }
            }
            FactValue::Key(v) => {
                big_embed::Fact::Key { field: &self.field, record: self.record, value: v }
            }
            FactValue::Bool(v) => {
                big_embed::Fact::Bool { field: &self.field, record: self.record, value: *v }
            }
            FactValue::Time { value, unix_seconds } => big_embed::Fact::Time {
                field: &self.field,
                record: self.record,
                value,
                unix_seconds: *unix_seconds,
            },
        }
    }

    /// The key this fact writes, if it writes one. What the coordinator collects before it
    /// sends anything anywhere.
    pub fn key(&self) -> Option<(&str, &str)> {
        match &self.value {
            // A timed key is a key: the string still has to be interned before the fact can be
            // written, and by the same leader.
            FactValue::Key(v) | FactValue::Time { value: v, .. } => {
                Some((self.field.as_str(), v.as_str()))
            }
            _ => None,
        }
    }
}

pub fn put_fact(out: &mut Vec<u8>, f: &OwnedFact) {
    put_str(out, &f.field);
    put_u64(out, f.record);
    match &f.value {
        FactValue::Int(v) => {
            put_u8(out, 0);
            put_u64(out, *v);
        }
        FactValue::Signed(v) => {
            put_u8(out, 1);
            put_i64(out, *v);
        }
        FactValue::Key(v) => {
            put_u8(out, 2);
            put_str(out, v);
        }
        FactValue::Bool(v) => {
            put_u8(out, 3);
            put_bool(out, *v);
        }
        FactValue::Time { value, unix_seconds } => {
            put_u8(out, 4);
            put_str(out, value);
            put_i64(out, *unix_seconds);
        }
        // Appended, never renumbered: an older peer answers `BadTag` on this rather than
        // reading it as one of the tags it does know.
        FactValue::Float(bits) => {
            put_u8(out, 5);
            put_u64(out, *bits);
        }
    }
}

pub fn get_fact(r: &mut Reader<'_>) -> Result<OwnedFact> {
    let field = r.str()?;
    let record = r.u64()?;
    let value = match r.u8()? {
        0 => FactValue::Int(r.u64()?),
        1 => FactValue::Signed(r.i64()?),
        2 => FactValue::Key(r.str()?),
        3 => FactValue::Bool(r.bool()?),
        4 => FactValue::Time { value: r.str()?, unix_seconds: r.i64()? },
        5 => FactValue::Float(r.u64()?),
        tag => return Err(WireError::BadTag { what: "fact", tag }),
    };
    Ok(OwnedFact { field, record, value })
}

/// What a key means, as decided by the schema leader.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Assignment {
    pub field: String,
    pub key: String,
    pub row: RowId,
}

/// One schema change, applied at the leader first and then at everybody else.
///
/// Field ids are not carried. They are a node's own numbering and nothing crosses the wire
/// that depends on two nodes agreeing about them - names do the resolving, all the way down.
/// Row ids are the only ids that have to mean the same thing everywhere, which is why they are
/// the only ones with a protocol.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ddl {
    CreateTable {
        table: String,
        engine: TableEngine,
    },
    CreateField {
        table: String,
        field: String,
        kind: FieldKind,
        bit_depth: u32,
    },
    CreateDecimal {
        table: String,
        field: String,
        bit_depth: u32,
        scale: i8,
    },
    CreateTimeQuantum {
        table: String,
        field: String,
        granularity: Vec<Granularity>,
    },
    DropTable {
        table: String,
    },
    DropField {
        table: String,
        field: String,
    },
    CreateDatabase {
        name: String,
    },
    /// Always the cascading form on the wire. Whether `CASCADE` was written is decided at the
    /// leader, which is where the table count that `RESTRICT` refuses on is authoritative; what
    /// travels to the other nodes is a change already ruled legal.
    DropDatabase {
        name: String,
    },
    /// A `SELECT` kept under a name. Always the replacing form on the wire: whether `OR REPLACE`
    /// was written is decided at the leader, where the existing definition is authoritative, and
    /// what travels to the other nodes is a change already ruled legal - the same argument
    /// [`Ddl::DropDatabase`] carries one variant up.
    CreateView {
        /// Qualified `database.view`, the single-string form every name here travels as.
        view: String,
        /// The statement, as it was written.
        text: String,
    },
    DropView {
        view: String,
    },
}

impl Ddl {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::CreateTable { table, engine } => {
                put_u8(&mut out, 0);
                put_str(&mut out, table);
                put_u8(&mut out, engine.code());
            }
            Self::CreateField { table, field, kind, bit_depth } => {
                put_u8(&mut out, 1);
                put_str(&mut out, table);
                put_str(&mut out, field);
                put_u8(&mut out, *kind as u8);
                put_u32(&mut out, *bit_depth);
            }
            Self::CreateDecimal { table, field, bit_depth, scale } => {
                put_u8(&mut out, 2);
                put_str(&mut out, table);
                put_str(&mut out, field);
                put_u32(&mut out, *bit_depth);
                put_u8(&mut out, *scale as u8);
            }
            Self::CreateTimeQuantum { table, field, granularity } => {
                put_u8(&mut out, 3);
                put_str(&mut out, table);
                put_str(&mut out, field);
                put_count(&mut out, granularity.len());
                for g in granularity {
                    put_u8(&mut out, g.as_char() as u8);
                }
            }
            Self::DropTable { table } => {
                put_u8(&mut out, 4);
                put_str(&mut out, table);
            }
            Self::DropField { table, field } => {
                put_u8(&mut out, 5);
                put_str(&mut out, table);
                put_str(&mut out, field);
            }
            Self::CreateDatabase { name } => {
                put_u8(&mut out, 6);
                put_str(&mut out, name);
            }
            Self::DropDatabase { name } => {
                put_u8(&mut out, 7);
                put_str(&mut out, name);
            }
            Self::CreateView { view, text } => {
                put_u8(&mut out, 8);
                put_str(&mut out, view);
                put_str(&mut out, text);
            }
            Self::DropView { view } => {
                put_u8(&mut out, 9);
                put_str(&mut out, view);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let out = match r.u8()? {
            0 => Self::CreateTable {
                table: r.str()?,
                engine: {
                    let e = r.u8()?;
                    TableEngine::from_u8(e)
                        .ok_or(WireError::BadTag { what: "table engine", tag: e })?
                },
            },
            1 => Self::CreateField {
                table: r.str()?,
                field: r.str()?,
                kind: {
                    let k = r.u8()?;
                    FieldKind::from_u8(k).ok_or(WireError::BadTag { what: "field kind", tag: k })?
                },
                bit_depth: r.u32()?,
            },
            2 => Self::CreateDecimal {
                table: r.str()?,
                field: r.str()?,
                bit_depth: r.u32()?,
                scale: r.u8()? as i8,
            },
            3 => Self::CreateTimeQuantum {
                table: r.str()?,
                field: r.str()?,
                granularity: {
                    let n = r.count()?;
                    let mut out = Vec::with_capacity(n);
                    for _ in 0..n {
                        let c = r.u8()?;
                        out.push(
                            granularity_of(c)
                                .ok_or(WireError::BadTag { what: "granularity", tag: c })?,
                        );
                    }
                    out
                },
            },
            4 => Self::DropTable { table: r.str()? },
            5 => Self::DropField { table: r.str()?, field: r.str()? },
            6 => Self::CreateDatabase { name: r.str()? },
            7 => Self::DropDatabase { name: r.str()? },
            8 => Self::CreateView { view: r.str()?, text: r.str()? },
            9 => Self::DropView { view: r.str()? },
            tag => return Err(WireError::BadTag { what: "schema change", tag }),
        };
        finished(&r)?;
        Ok(out)
    }
}

/// The inverse of [`Granularity::as_char`], which has no inverse of its own because nothing
/// below this crate ever had to read one back.
pub(super) fn granularity_of(c: u8) -> Option<Granularity> {
    Some(match c {
        b'Y' => Granularity::Year,
        b'M' => Granularity::Month,
        b'D' => Granularity::Day,
        b'H' => Granularity::Hour,
        _ => return None,
    })
}
