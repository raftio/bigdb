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

//! Set and bool fields. One row per distinct value; a record may sit in several rows.

use crate::bitmap::field::error::Result;
use crate::bitmap::{FragmentRead, FragmentWrite, RecordId, RowId, RowSet};
use big_pager::{Pager, PagerMut, WriteTxn};

pub struct SetField;

impl SetField {
    pub fn set<P: PagerMut>(
        txn: &mut WriteTxn<'_, P>,
        f: &mut FragmentWrite,
        row: RowId,
        record: RecordId,
    ) -> Result<()> {
        f.set_bits(txn, [(row, record)])?;
        Ok(())
    }

    pub fn clear<P: PagerMut>(
        txn: &mut WriteTxn<'_, P>,
        f: &mut FragmentWrite,
        row: RowId,
        record: RecordId,
    ) -> Result<()> {
        f.clear_bits(txn, [(row, record)])?;
        Ok(())
    }

    pub fn row<P: Pager>(f: &FragmentRead<'_, P>, row: RowId) -> Result<RowSet> {
        Ok(f.row(row)?)
    }

    pub fn count<P: Pager>(f: &FragmentRead<'_, P>, row: RowId) -> Result<u64> {
        Ok(f.row_count(row)?)
    }

    /// Rows a record belongs to. Costs one probe per row, which is the price of a set field
    /// having no reverse map; a mutex field pays for one and gets this cheaply.
    pub fn rows_of<P: Pager>(f: &FragmentRead<'_, P>, record: RecordId) -> Result<Vec<RowId>> {
        let mut out = Vec::new();
        for row in f.rows()? {
            if f.get(row, record)? {
                out.push(row);
            }
        }
        Ok(out)
    }
}

/// A mutex field with exactly two rows.
pub struct BoolField;

impl BoolField {
    pub const FALSE_ROW: RowId = 0;
    pub const TRUE_ROW: RowId = 1;

    pub fn row_of(v: bool) -> RowId {
        if v {
            Self::TRUE_ROW
        } else {
            Self::FALSE_ROW
        }
    }

    pub fn set<P: PagerMut>(
        txn: &mut WriteTxn<'_, P>,
        f: &mut FragmentWrite,
        record: RecordId,
        value: bool,
    ) -> Result<()> {
        f.clear_bits(txn, [(Self::row_of(!value), record)])?;
        f.set_bits(txn, [(Self::row_of(value), record)])?;
        Ok(())
    }

    pub fn get<P: Pager>(f: &FragmentRead<'_, P>, record: RecordId) -> Result<Option<bool>> {
        if f.get(Self::TRUE_ROW, record)? {
            return Ok(Some(true));
        }
        if f.get(Self::FALSE_ROW, record)? {
            return Ok(Some(false));
        }
        Ok(None)
    }
}
