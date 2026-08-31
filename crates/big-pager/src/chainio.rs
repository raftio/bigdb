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

//! Reads a chained flat list of pages. Cycle-guarded, because `next` comes off disk.

use crate::error::{Result, StoreError};
use crate::pager::Pager;
use big_page::{ChainPage, PageType, Pgno};

pub fn load_chain<P: Pager>(
    pager: &P,
    root: Option<Pgno>,
    ty: PageType,
    stride: usize,
) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut cur = root;
    let mut hops = 0u64;
    let limit = pager.page_count() + 1;

    while let Some(pgno) = cur {
        hops += 1;
        if hops > limit {
            return Err(StoreError::ChainCycle { root: root.unwrap_or(0) });
        }
        let page = pager.read(pgno)?;
        page.verify_checksum()?;
        let chain = ChainPage::parse(&page, ty, stride)?;
        out.extend(chain.entries().map(|e| e.to_vec()));
        cur = chain.next();
    }
    Ok(out)
}

/// Walks the chain and returns every page number, needed to hand them back to the freelist
/// when the chain is rewritten.
pub fn chain_pgnos<P: Pager>(
    pager: &P,
    root: Option<Pgno>,
    ty: PageType,
    stride: usize,
) -> Result<Vec<Pgno>> {
    let mut out = Vec::new();
    let mut cur = root;
    let limit = pager.page_count() + 1;

    while let Some(pgno) = cur {
        if out.len() as u64 > limit {
            return Err(StoreError::ChainCycle { root: root.unwrap_or(0) });
        }
        out.push(pgno);
        let page = pager.read(pgno)?;
        cur = ChainPage::parse(&page, ty, stride)?.next();
    }
    Ok(out)
}
