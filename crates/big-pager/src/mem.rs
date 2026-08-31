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

//! An in-RAM pager for b-tree unit tests: no file, no transaction needed.
//! Its `Ref` is a real guard rather than a `&Page`, which exercises the trait's GAT.

use crate::error::{Result, StoreError};
use crate::pager::{Pager, PagerMut};
use big_page::{Page, Pgno};
use std::sync::RwLock;

#[derive(Default)]
pub struct MemPager {
    pages: RwLock<Vec<Page>>,
}

/// Deep copy: reload tests need a second handle onto the same bytes.
impl Clone for MemPager {
    fn clone(&self) -> Self {
        Self { pages: RwLock::new(self.pages.read().unwrap().clone()) }
    }
}

impl MemPager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_pages(n: u64) -> Self {
        Self { pages: RwLock::new(vec![Page::zeroed(); n as usize]) }
    }
}

/// Holds the read lock for the reference's whole life, so the `Vec` cannot realloc under it.
pub struct MemRef<'a> {
    guard: std::sync::RwLockReadGuard<'a, Vec<Page>>,
    idx: usize,
}

impl core::ops::Deref for MemRef<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        &self.guard[self.idx]
    }
}

impl Pager for MemPager {
    type Ref<'a> = MemRef<'a>;

    fn read(&self, pgno: Pgno) -> Result<MemRef<'_>> {
        let guard = self.pages.read().unwrap();
        let page_count = guard.len() as u64;
        if pgno as u64 >= page_count {
            return Err(StoreError::OutOfBounds { pgno, page_count });
        }
        Ok(MemRef { guard, idx: pgno as usize })
    }

    fn page_count(&self) -> u64 {
        self.pages.read().unwrap().len() as u64
    }
}

impl PagerMut for MemPager {
    fn write(&self, pgno: Pgno, page: &Page) -> Result<()> {
        let mut guard = self.pages.write().unwrap();
        let page_count = guard.len() as u64;
        let slot =
            guard.get_mut(pgno as usize).ok_or(StoreError::OutOfBounds { pgno, page_count })?;
        *slot = page.clone();
        Ok(())
    }

    fn grow(&self, page_count: u64) -> Result<()> {
        let mut guard = self.pages.write().unwrap();
        if page_count > guard.len() as u64 {
            guard.resize(page_count as usize, Page::zeroed());
        }
        Ok(())
    }

    fn truncate(&self, page_count: u64) -> Result<()> {
        let mut guard = self.pages.write().unwrap();
        if page_count < guard.len() as u64 {
            guard.truncate(page_count as usize);
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }
}
