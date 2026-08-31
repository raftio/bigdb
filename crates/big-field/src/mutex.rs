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

//! Mutex field: at most one row per record.
//!
//! Clearing the old bit means knowing which row a record currently sits in. Without a reverse
//! map that is a probe per row, so every mutex field carries a shadow BSI holding the current
//! row id: `O(bit_depth)` instead of `O(rows)`, and the same structure also makes a point
//! lookup of the value cheap. One mechanism, two problems.
//!
//! The shadow lives in its own fragment rather than in reserved rows of the value fragment, so
//! the value row space stays exactly what the user declared.

use crate::bsi::Bsi;
use crate::error::Result;
use big_fragment::{FragmentRead, FragmentWrite, RecordId, RowId, RowSet};
use big_pager::{Pager, PagerMut, WriteTxn};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MutexField {
    /// Wide enough to hold the largest row id the field will ever have.
    pub shadow: Bsi,
}

impl MutexField {
    pub fn new(row_bit_depth: u32) -> Self {
        Self { shadow: Bsi::new(row_bit_depth) }
    }

    /// Moves a record into `row`, clearing whichever row it was in before.
    ///
    /// Maintaining the shadow costs `bit_depth` container touches per write, which is only
    /// affordable in a batch. It buys back a probe over every row on each write.
    pub fn put<P: PagerMut>(
        &self,
        txn: &mut WriteTxn<'_, P>,
        values: &mut FragmentWrite,
        shadow: &mut FragmentWrite,
        record: RecordId,
        row: RowId,
    ) -> Result<()> {
        let old = match shadow.reader(txn) {
            Some(r) => self.shadow.get(&r, record)?,
            None => None,
        };
        if let Some(old) = old {
            if old != row {
                values.clear_bits(txn, [(old, record)])?;
            }
        }
        values.set_bits(txn, [(row, record)])?;
        self.shadow.set(txn, shadow, record, row)?;
        Ok(())
    }

    /// Removes the record from the field entirely.
    pub fn clear<P: PagerMut>(
        &self,
        txn: &mut WriteTxn<'_, P>,
        values: &mut FragmentWrite,
        shadow: &mut FragmentWrite,
        record: RecordId,
    ) -> Result<Option<RowId>> {
        let old = match shadow.reader(txn) {
            Some(r) => self.shadow.get(&r, record)?,
            None => None,
        };
        if let Some(old) = old {
            values.clear_bits(txn, [(old, record)])?;
            self.shadow.clear(txn, shadow, record)?;
        }
        Ok(old)
    }

    /// Point lookup straight out of the shadow: `O(bit_depth)`, not `O(rows)`.
    pub fn get<P: Pager>(
        &self,
        shadow_read: &FragmentRead<'_, P>,
        record: RecordId,
    ) -> Result<Option<RowId>> {
        self.shadow.get(shadow_read, record)
    }

    pub fn row<P: Pager>(&self, values: &FragmentRead<'_, P>, row: RowId) -> Result<RowSet> {
        Ok(values.row(row)?)
    }

    /// Cross-check for tests and repair: the value fragment must agree with the shadow.
    pub fn verify<P: Pager, Q: Pager>(
        &self,
        values: &FragmentRead<'_, P>,
        shadow_read: &FragmentRead<'_, Q>,
        record: RecordId,
    ) -> Result<()> {
        let claimed = self.shadow.get(shadow_read, record)?;
        let mut actual = None;
        for row in values.rows()? {
            if values.get(row, record)? {
                if actual.is_some() {
                    return Err(crate::error::FieldError::MutexConflict { record });
                }
                actual = Some(row);
            }
        }
        if actual != claimed {
            return Err(crate::error::FieldError::MutexConflict { record });
        }
        Ok(())
    }
}
