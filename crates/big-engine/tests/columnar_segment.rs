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

//! A segment against a real pager: blocks through the tree, and the pages they own.

use big_engine::columnar::*;
use big_pager::{MemPager, Store};
use core::ops::ControlFlow;

/// Runs `body` inside one committed transaction and hands back the segment's root.
fn write(
    store: &Store<MemPager>,
    root: Option<Pgno>,
    body: impl FnOnce(&mut ColumnWrite, &mut big_pager::WriteTxn<'_, MemPager>),
) -> Option<Pgno> {
    let mut txn = store.begin_write();
    let mut w = ColumnWrite::new(root);
    body(&mut w, &mut txn);
    let out = w.root();
    txn.commit().unwrap();
    out
}

fn scalar_block(values: &[(usize, u64)]) -> Block {
    let mut b = Block::new();
    for (slot, v) in values {
        b.set(*slot, Cell::Value(*v));
    }
    b
}

#[test]
fn a_block_survives_the_tree() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let block = scalar_block(&[(0, 10), (5, 20), (1023, 30)]);

    let root = write(&store, None, |w, txn| w.write_block(txn, 0, &block).unwrap());

    let read = store.begin_read();
    let seg = ColumnRead::new(store.pager(), root.unwrap());
    assert_eq!(seg.block(0).unwrap(), block);
    assert_eq!(seg.cell(5).unwrap(), Cell::Value(20));
    assert_eq!(seg.cell(6).unwrap(), Cell::Null);
    assert_eq!(seg.count().unwrap(), 3);
    drop(read);
}

/// A block wide enough to need a page of its own goes through the same dense-page arrangement a
/// bitmap uses, and comes back verified against the checksum in the cell above it.
#[test]
fn a_spilled_block_survives_the_tree() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let mut block = Block::new();
    for i in 0..BLOCK_RECORDS as usize {
        block.set(i, Cell::Value((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)));
    }

    let root = write(&store, None, |w, txn| w.write_block(txn, 3, &block).unwrap());
    let seg = ColumnRead::new(store.pager(), root.unwrap());
    assert_eq!(seg.block(3).unwrap(), block);
    assert_eq!(seg.blocks().unwrap(), vec![3]);
}

#[test]
fn a_list_block_survives_the_tree() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let mut block = Block::new();
    for i in 0..500usize {
        block.set(i, Cell::List((0..(i % 5 + 1) as u64).map(|k| k * 7 + i as u64).collect()));
    }

    let root = write(&store, None, |w, txn| w.write_block(txn, 0, &block).unwrap());
    let seg = ColumnRead::new(store.pager(), root.unwrap());
    assert_eq!(seg.block(0).unwrap(), block);
    // The count is of *records* holding values, not of the values themselves - only the first
    // part of a block carries it, which is what keeps the value parts from inflating it.
    assert_eq!(seg.count().unwrap(), 500);
}

/// A block that shrinks must drop the parts it no longer needs. A stale value cell left behind
/// is not dead weight: the next read would splice its values into some record's list.
#[test]
fn a_shrinking_list_block_drops_its_stale_parts() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let mut big = Block::new();
    for i in 0..1000usize {
        big.set(i, Cell::List((0..4u64).map(|k| k + i as u64 * 10).collect()));
    }
    let root = write(&store, None, |w, txn| w.write_block(txn, 0, &big).unwrap());

    let mut small = Block::new();
    small.set(0, Cell::List(vec![1, 2]));
    let root = write(&store, root, |w, txn| w.write_block(txn, 0, &small).unwrap());

    let seg = ColumnRead::new(store.pager(), root.unwrap());
    assert_eq!(seg.block(0).unwrap(), small, "stale value parts survived the shrink");
}

/// Clearing every slot leaves no tree at all, rather than an empty leaf holding a page and a
/// root record that nothing can reach.
#[test]
fn a_block_cleared_to_nothing_collapses_the_tree() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let root =
        write(&store, None, |w, txn| w.write_block(txn, 0, &scalar_block(&[(1, 7)])).unwrap());
    assert!(root.is_some());

    let root = write(&store, root, |w, txn| w.write_block(txn, 0, &Block::new()).unwrap());
    assert_eq!(root, None, "an emptied segment must have no root");
}

#[test]
fn blocks_are_visited_in_order_and_only_once() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let mut root = None;
    for block in [7u64, 1, 40, 0] {
        root = write(&store, root, |w, txn| {
            w.write_block(txn, block, &scalar_block(&[(0, block)])).unwrap()
        });
    }

    let seg = ColumnRead::new(store.pager(), root.unwrap());
    assert_eq!(seg.blocks().unwrap(), vec![0, 1, 7, 40]);

    let mut seen = Vec::new();
    seg.for_each_block(|i, b| {
        seen.push((i, b.get(0).value()));
        Ok(ControlFlow::Continue(()))
    })
    .unwrap();
    assert_eq!(seen, vec![(0, Some(0)), (1, Some(1)), (7, Some(7)), (40, Some(40))]);
}

/// `edit_block` is the read-modify-write a single-record change needs, and it must see what the
/// same transaction has already written rather than what was committed before it.
#[test]
fn an_edit_sees_the_transactions_own_earlier_writes() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let mut txn = store.begin_write();
    let mut w = ColumnWrite::new(None);

    w.edit_block(&mut txn, 0, |b| b.set(1, Cell::Value(11))).unwrap();
    w.edit_block(&mut txn, 0, |b| b.set(2, Cell::Value(22))).unwrap();

    let seen = w.reader(&txn).unwrap().block(0).unwrap();
    assert_eq!(seen.get(1), &Cell::Value(11), "the second edit lost the first");
    assert_eq!(seen.get(2), &Cell::Value(22));
    txn.commit().unwrap();
}

/// An edit that changes nothing must not rewrite the block. Without this a delete of a record
/// that was never there would dirty every block it touched.
#[test]
fn an_edit_that_changes_nothing_writes_nothing() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let root =
        write(&store, None, |w, txn| w.write_block(txn, 0, &scalar_block(&[(1, 7)])).unwrap());
    let before = store.metrics().page_count;

    let after_root =
        write(&store, root, |w, txn| w.edit_block(txn, 0, |b| b.set(1, Cell::Value(7))).unwrap());
    assert_eq!(after_root, root);
    assert_eq!(store.metrics().page_count, before, "an unchanged block was rewritten");
}

/// The pages a segment owns have to be reachable from the walk, or a backup would lose them and
/// a drop would leak them. This is the property `ContainerType::owns_page` exists to guarantee.
#[test]
fn a_spilled_block_is_reachable_from_the_walk() {
    let store = Store::open_or_init(MemPager::new()).unwrap();
    let mut block = Block::new();
    for i in 0..BLOCK_RECORDS as usize {
        block.set(i, Cell::Value((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)));
    }
    let root = write(&store, None, |w, txn| w.write_block(txn, 0, &block).unwrap()).unwrap();

    let mut pages = Vec::new();
    big_btree::visit_tree(store.pager(), root, &mut |pgno, _| {
        pages.push(pgno);
        Ok(())
    })
    .unwrap();
    // The leaf and the values page. If `owns_page` missed the values type this would be one.
    assert_eq!(pages.len(), 2, "the values page is not reachable from the walk: {pages:?}");

    // And the scrub verifies it, which is what makes a rotted values page findable at all.
    let scrubbed = big_btree::scrub_tree(store.pager(), root).unwrap();
    assert!(scrubbed.total() >= 2, "{scrubbed:?}");
}
