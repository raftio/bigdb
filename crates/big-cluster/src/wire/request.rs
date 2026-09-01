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

//! One type per `/internal` route, and the small bodies that answer them.
//!
//! Each request type owns its `encode`/`decode` pair, so a route's two ends are read together.
//! The bodies that are just a list or a number live at the bottom - they have no request type
//! because there is nothing to name.

use super::*;

/// `POST /internal/query`: run this plan over the shards you own.
///
/// The deadline travels as what is left of it rather than as a wall-clock instant, because two
/// machines' clocks are not the same clock and the difference would silently become part of
/// every budget.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct QueryRequest {
    pub plan: Plan,
    /// Milliseconds left of the coordinator's budget; `None` for no budget.
    pub timeout_ms: Option<u64>,
}

impl QueryRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_plan(&mut out, &self.plan);
        put_opt_u64(&mut out, self.timeout_ms);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let out = Self { plan: get_plan(&mut r)?, timeout_ms: r.opt_u64()? };
        finished(&r)?;
        Ok(out)
    }
}

/// `POST /internal/import`: write these facts, and take these row ids as given.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImportRequest {
    pub table: String,
    pub keys: Vec<Assignment>,
    pub facts: Vec<OwnedFact>,
}

impl ImportRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.table);
        put_count(&mut out, self.keys.len());
        for k in &self.keys {
            put_str(&mut out, &k.field);
            put_str(&mut out, &k.key);
            put_u64(&mut out, k.row);
        }
        put_count(&mut out, self.facts.len());
        for f in &self.facts {
            put_fact(&mut out, f);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let table = r.str()?;
        let n = r.count()?;
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            keys.push(Assignment { field: r.str()?, key: r.str()?, row: r.u64()? });
        }
        let n = r.count()?;
        let mut facts = Vec::with_capacity(n);
        for _ in 0..n {
            facts.push(get_fact(&mut r)?);
        }
        finished(&r)?;
        Ok(Self { table, keys, facts })
    }
}

/// `POST /internal/delete`: remove these records, all of which this node owns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeleteRequest {
    pub table: String,
    pub records: Vec<RecordId>,
}

impl DeleteRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.table);
        put_count(&mut out, self.records.len());
        for r in &self.records {
            put_u64(&mut out, *r);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let table = r.str()?;
        let n = r.count()?;
        let mut records = Vec::with_capacity(n);
        for _ in 0..n {
            records.push(r.u64()?);
        }
        finished(&r)?;
        Ok(Self { table, records })
    }
}

/// `POST /internal/records`: a page of record ids from the shards you own.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RecordsRequest {
    pub table: String,
    pub after: Option<RecordId>,
    pub limit: u64,
}

impl RecordsRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.table);
        put_opt_u64(&mut out, self.after);
        put_u64(&mut out, self.limit);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let out = Self { table: r.str()?, after: r.opt_u64()?, limit: r.u64()? };
        finished(&r)?;
        Ok(out)
    }
}

/// `POST /internal/intern`: what do these keys mean. Only the schema leader answers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InternRequest {
    pub table: String,
    pub field: String,
    pub keys: Vec<String>,
}

impl InternRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.table);
        put_str(&mut out, &self.field);
        put_count(&mut out, self.keys.len());
        for k in &self.keys {
            put_str(&mut out, k);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let table = r.str()?;
        let field = r.str()?;
        let n = r.count()?;
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            keys.push(r.str()?);
        }
        finished(&r)?;
        Ok(Self { table, field, keys })
    }
}

/// `POST /internal/allocate`: a run of record ids nobody else will be given.
///
/// Only the schema leader answers, for the reason `/internal/intern` exists: two coordinators
/// handing out one id would write two records into one, and nothing downstream could see that
/// it had happened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AllocateRequest {
    pub table: String,
    /// How many consecutive ids the caller needs. A whole statement asks once.
    pub count: u64,
}

impl AllocateRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.table);
        put_u64(&mut out, self.count);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let table = r.str()?;
        let count = r.u64()?;
        finished(&r)?;
        Ok(Self { table, count })
    }
}

/// `POST /internal/next-record`: one past the highest record id this node holds, or zero.
///
/// A table name and nothing else. One past the highest rather than the highest itself, so that
/// an empty table and a table holding record zero are told apart without an `Option` on the
/// wire.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TableRequest {
    pub table: String,
}

impl TableRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.table);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let table = r.str()?;
        finished(&r)?;
        Ok(Self { table })
    }
}

/// A list of record ids, which is what `/internal/records` answers with.
pub fn put_records(out: &mut Vec<u8>, ids: &[RecordId]) {
    put_count(out, ids.len());
    for id in ids {
        put_u64(out, *id);
    }
}

pub fn get_records(bytes: &[u8]) -> Result<Vec<RecordId>> {
    let mut r = Reader::new(bytes);
    let n = r.count()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.u64()?);
    }
    finished(&r)?;
    Ok(out)
}

/// A list of row ids, which is what `/internal/intern` answers with.
pub fn put_rows_ids(out: &mut Vec<u8>, rows: &[RowId]) {
    put_records(out, rows);
}

pub fn get_rows_ids(bytes: &[u8]) -> Result<Vec<RowId>> {
    get_records(bytes)
}

/// One number, which is what `/internal/delete` answers with.
pub fn put_u64_body(v: u64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

pub fn get_u64_body(bytes: &[u8]) -> Result<u64> {
    let mut r = Reader::new(bytes);
    let v = r.u64()?;
    finished(&r)?;
    Ok(v)
}

// ---------------------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------------------

/// One fragment, named the way every node can resolve for itself.
pub fn put_addr(out: &mut Vec<u8>, a: &FragmentAddr) {
    put_str(out, &a.table);
    put_opt_str(out, a.field.as_deref());
    put_opt_str(out, a.view.as_deref());
    put_u32(out, a.field_id);
    put_u32(out, a.view_id);
    put_u64(out, a.shard);
}

pub fn get_addr(r: &mut Reader<'_>) -> Result<FragmentAddr> {
    Ok(FragmentAddr {
        table: r.str()?,
        field: r.opt_str()?,
        view: r.opt_str()?,
        field_id: r.u32()?,
        view_id: r.u32()?,
        shard: r.u64()?,
    })
}

/// The zone map and bit depth, which have to travel with the bits.
///
/// A copy whose zone map is missing would skip shards that hold matches: `amount > k` reads
/// the map before it reads a page, so a repair that carried only the containers would leave a
/// database that is correct in storage and wrong in every range query.
pub fn put_meta(out: &mut Vec<u8>, m: &FragmentMeta) {
    put_u32(out, m.bit_depth);
    put_u64(out, m.min);
    put_u64(out, m.max);
    put_bool(out, m.has_values);
}

pub fn get_meta(r: &mut Reader<'_>) -> Result<FragmentMeta> {
    Ok(FragmentMeta { bit_depth: r.u32()?, min: r.u64()?, max: r.u64()?, has_values: r.bool()? })
}

/// `POST /internal/fragments`: what do you hold for this table, and how much of it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FragmentsRequest {
    pub table: String,
}

impl FragmentsRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.table);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let out = Self { table: r.str()? };
        finished(&r)?;
        Ok(out)
    }
}

/// The answer: every fragment, with the count that stands in for its contents.
pub fn put_fragment_list(out: &mut Vec<u8>, list: &[(FragmentAddr, u64)]) {
    put_count(out, list.len());
    for (addr, count) in list {
        put_addr(out, addr);
        put_u64(out, *count);
    }
}

pub fn get_fragment_list(bytes: &[u8]) -> Result<Vec<(FragmentAddr, u64)>> {
    let mut r = Reader::new(bytes);
    let n = r.count()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push((get_addr(&mut r)?, r.u64()?));
    }
    finished(&r)?;
    Ok(out)
}

/// One fragment, whole: what `POST /internal/fragment` answers and what
/// `POST /internal/fragment/put` takes.
#[derive(Clone, Debug)]
pub struct FragmentBody {
    pub addr: FragmentAddr,
    pub meta: FragmentMeta,
    /// Containers or cells, decided by which view the address names. See
    /// [`big_api::FragmentData`].
    pub data: FragmentData,
}

/// Which units a body carries. Written before the units themselves so a reader knows what it
/// is about to decode rather than inferring it from the address - an address is resolved
/// against the *receiver's* catalog, and a body has to be readable before that resolution.
mod data_tag {
    pub const CONTAINERS: u8 = 0;
    pub const CELLS: u8 = 1;
}

/// Which shape one cell is. A scalar column and a keyed one store different things and a reader
/// cannot tell them apart from the value alone.
mod cell_tag {
    pub const VALUE: u8 = 0;
    pub const LIST: u8 = 1;
}

impl FragmentBody {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_addr(&mut out, &self.addr);
        put_meta(&mut out, &self.meta);
        match &self.data {
            FragmentData::Containers(containers) => {
                put_u8(&mut out, data_tag::CONTAINERS);
                put_count(&mut out, containers.len());
                for (ckey, c) in containers {
                    put_u64(&mut out, *ckey);
                    put_container(&mut out, c.as_ref());
                }
            }
            FragmentData::Cells(cells) => {
                put_u8(&mut out, data_tag::CELLS);
                put_count(&mut out, cells.len());
                for (local, cell) in cells {
                    put_u64(&mut out, *local);
                    match cell {
                        ColumnCell::Value(v) => {
                            put_u8(&mut out, cell_tag::VALUE);
                            put_u64(&mut out, *v);
                        }
                        // A null is never sent: `segment_cells` skips them, because an absent
                        // cell and a cell that is absent are the same thing and the receiver
                        // starts from a segment it has just discarded.
                        ColumnCell::Null | ColumnCell::List(_) => {
                            put_u8(&mut out, cell_tag::LIST);
                            let rows = cell.list();
                            put_count(&mut out, rows.len());
                            for row in rows {
                                put_u64(&mut out, *row);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let addr = get_addr(&mut r)?;
        let meta = get_meta(&mut r)?;
        let tag = r.u8()?;
        let n = r.count()?;
        let data = match tag {
            data_tag::CONTAINERS => {
                let mut containers = Vec::with_capacity(n);
                for _ in 0..n {
                    containers.push((r.u64()?, get_container(&mut r)?));
                }
                FragmentData::Containers(containers)
            }
            data_tag::CELLS => {
                let mut cells = Vec::with_capacity(n);
                for _ in 0..n {
                    let local = r.u64()?;
                    let cell = match r.u8()? {
                        cell_tag::VALUE => ColumnCell::Value(r.u64()?),
                        cell_tag::LIST => {
                            let k = r.count()?;
                            let mut rows = Vec::with_capacity(k);
                            for _ in 0..k {
                                rows.push(r.u64()?);
                            }
                            ColumnCell::List(rows)
                        }
                        other => return Err(WireError::BadTag { what: "column cell", tag: other }),
                    };
                    cells.push((local, cell));
                }
                FragmentData::Cells(cells)
            }
            other => return Err(WireError::BadTag { what: "fragment data", tag: other }),
        };
        finished(&r)?;
        Ok(Self { addr, meta, data })
    }
}

/// `POST /internal/fragment`: send me this one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FragmentRequest {
    pub addr: FragmentAddr,
}

impl FragmentRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_addr(&mut out, &self.addr);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let out = Self { addr: get_addr(&mut r)? };
        finished(&r)?;
        Ok(out)
    }
}

/// Every row key of a table: what one copy has to be told before its bits mean anything.
///
/// A fragment is rows of bits, and a row is a number until something says which string it
/// stands for. A repair that carried the bits and not the mapping would leave a copy that
/// answers `Count` correctly and `GroupBy` with nothing but nulls.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeysBody {
    pub table: String,
    pub keys: Vec<Assignment>,
}

impl KeysBody {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, &self.table);
        put_count(&mut out, self.keys.len());
        for k in &self.keys {
            put_str(&mut out, &k.field);
            put_str(&mut out, &k.key);
            put_u64(&mut out, k.row);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let table = r.str()?;
        let n = r.count()?;
        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            keys.push(Assignment { field: r.str()?, key: r.str()?, row: r.u64()? });
        }
        finished(&r)?;
        Ok(Self { table, keys })
    }
}

/// The schema, as one node holds it: its tables, then its views.
///
/// Carried so that a repair can make a copy's schema match before it tries to make its bits
/// match: a fragment cannot land on a node that has never heard of the field it belongs to.
///
/// **Views are in the exchange even though they hold no bits**, because a repaired node that
/// lacked one would refuse a statement its peers answer - and which node a client reached would
/// decide whether the statement worked. That is the class of failure this whole layer exists to
/// prevent. Appended after the tables, which is the shape change [`crate::WIRE_VERSION`] `4` is.
pub fn put_schema(out: &mut Vec<u8>, tables: &[big_api::TableInfo], views: &[big_api::ViewInfo]) {
    put_count(out, tables.len());
    for table in tables {
        // Qualified, so a repair recreates the table in the database the sender had it in.
        // Bare for the default database, which is every table a `2` node ever had - so a
        // schema from one still reads as tables in `default`, which is where they are.
        put_str(out, &big_db::TableRef::new(&table.database, &table.name).to_string());
        put_u8(out, table.engine.code());
        put_count(out, table.fields.len());
        for field in &table.fields {
            put_str(out, &field.name);
            put_u8(out, field.kind as u8);
            put_u32(out, field.bit_depth);
            put_u8(out, field.scale as u8);
            put_count(out, field.granularity.len());
            for g in &field.granularity {
                put_u8(out, g.as_char() as u8);
            }
        }
    }
    put_count(out, views.len());
    for view in views {
        // Qualified for the reason a table is, and by the same helper: a repair has to recreate
        // the view in the database the sender had it in, since that is also the database its
        // body resolves in.
        put_str(out, &view.qualified());
        put_str(out, &view.text);
    }
}

pub fn get_schema(bytes: &[u8]) -> Result<(Vec<big_api::TableInfo>, Vec<big_api::ViewInfo>)> {
    let mut r = Reader::new(bytes);
    let n = r.count()?;
    let mut tables = Vec::with_capacity(n);
    for _ in 0..n {
        let name = r.str()?;
        let e = r.u8()?;
        let engine =
            TableEngine::from_u8(e).ok_or(WireError::BadTag { what: "table engine", tag: e })?;
        let count = r.count()?;
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            let field = r.str()?;
            let k = r.u8()?;
            let kind =
                FieldKind::from_u8(k).ok_or(WireError::BadTag { what: "field kind", tag: k })?;
            let bit_depth = r.u32()?;
            let scale = r.u8()? as i8;
            let n = r.count()?;
            let mut granularity = Vec::with_capacity(n);
            for _ in 0..n {
                let c = r.u8()?;
                granularity.push(
                    granularity_of(c).ok_or(WireError::BadTag { what: "granularity", tag: c })?,
                );
            }
            fields.push(big_api::FieldInfo { name: field, kind, bit_depth, scale, granularity });
        }
        let r = big_db::TableRef::parse(&name);
        tables.push(big_api::TableInfo {
            database: r.database.to_string(),
            name: r.table.to_string(),
            engine,
            fields,
        });
    }
    let n = r.count()?;
    let mut views = Vec::with_capacity(n);
    for _ in 0..n {
        let name = r.str()?;
        let text = r.str()?;
        let q = big_db::TableRef::parse(&name);
        views.push(big_api::ViewInfo {
            database: q.database.to_string(),
            name: q.table.to_string(),
            text,
        });
    }
    finished(&r)?;
    Ok((tables, views))
}

/// `POST /internal/repaired`: this copy has caught up, so stop refusing to promote it.
pub fn put_node(node: usize) -> Vec<u8> {
    put_u64_body(node as u64)
}

pub fn get_node(bytes: &[u8]) -> Result<usize> {
    get_u64_body(bytes).map(|v| v as usize)
}
