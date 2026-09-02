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

//! The four shapes that produce one row per something: a group, a pair, a join, and the cut
//! that every one of them ends with.
//!
//! They are kept apart rather than generalised because their drivers differ in the one way that
//! matters - a group is found by a row id, a pair by two of them, and a join by a key's string -
//! and a lookup that took "some rows" would be a lookup nobody could read.

use super::{Datum, Row};
use crate::{
    Absent, Cell, Cut, Group, GroupOrder, Having, JoinSide, Of, OrderBy, Pair, Pairing, RowId,
    Value,
};

use super::num::{as_f64, cmp_num, int_of, number, scalar_num, Num};

/// The rows of a grouped answer, joined across every plan the statement made.
///
/// **The join is on the row id, not the key.** A row id is assigned once for the whole cluster
/// and never reused; a key is a string a particular node may not have been told, and joining on
/// one would fuse every group whose key is `null` into one. The driver is the first plan's
/// group list, because that is the list whose order the answer is in - a `TopN` ranks its own
/// groups, and re-sorting them here would undo the ranking.
pub(super) fn grouped(
    keys: &[usize],
    cells: &[Cell],
    having: Option<&Having>,
    order: Option<GroupOrder>,
    cut: Cut,
    values: &[Value],
) -> Vec<Row> {
    // **Only the plans this shape names.** With `UNION ALL` the statement's list holds other
    // branches' answers too, and a driver built from all of them would give this branch rows
    // about groups it never asked for.
    let of_each: Vec<&[Group]> =
        keys.iter().filter_map(|i| values.get(*i)).map(|v| v.as_groups().unwrap_or(&[])).collect();

    // Every row any plan produced, in the first plan's order. The plans are grouped on the same
    // field over the same records, so the tail is normally empty - it exists so that a plan
    // that did answer about a group cannot have its number dropped because another did not.
    let mut rows: Vec<RowId> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for groups in &of_each {
        for g in *groups {
            if seen.insert(g.row) {
                rows.push(g.row);
            }
        }
    }
    // With one plan the order is that plan's own, which for a `TopN` is the ranking it was
    // asked for and must not be undone. With several, each arrives in key order and the union
    // of them is not - so it is put back into the order every one of them was already in.
    if of_each.len() > 1 {
        rows.sort_by_key(|row| {
            let (unnamed, key, row) = key_order(&of_each, *row);
            (unnamed, key.map(str::to_string), row)
        });
    }

    // `HAVING`, then `ORDER BY`, then `OFFSET`, then `LIMIT` - the order SQL specifies, and the
    // only one that is right here. A limit applied before the predicate would count rows the
    // predicate is about to drop, and an offset applied before the sort would skip into a list
    // nobody asked for.
    let mut kept: Vec<RowId> = rows
        .into_iter()
        .filter(|row| match having {
            Some(h) => h.holds(&|of| int_of(number(of, values, Some(*row)))),
            None => true,
        })
        .collect();

    if let Some(o) = order {
        sort_rows(&mut kept, o, &of_each, values);
    }

    // `WITH TIES` keeps every further row the ordering cannot tell apart from the last one
    // inside the limit. Without an ordering there is nothing to tie on, which the parser
    // refuses - so `false` here is unreachable rather than a silent "no ties".
    let ties = |a: &RowId, b: &RowId| match order {
        None => false,
        Some(o) => match o.by {
            OrderBy::Key => key_order(&of_each, *a) == key_order(&of_each, *b),
            OrderBy::Value { of } => number(of, values, Some(*a)) == number(of, values, Some(*b)),
        },
    };

    cut_rows(kept, cut, ties)
        .into_iter()
        .map(|row| {
            cells
                .iter()
                .map(|c| match c.of {
                    // A row with no interned name is a `null`, not an empty string: it is a
                    // group whose key this node has never been told, and the two are different
                    // facts.
                    Of::Key => find(&of_each, row)
                        .and_then(|g| g.key.as_deref())
                        .map_or(Datum::Null, Datum::text),
                    of => Datum::num(number(of, values, Some(row)), &c.units),
                })
                .collect()
        })
        .collect()
}

/// The rows of a pair grouping: one per combination two columns were both held by.
///
/// The same shape as [`grouped`] with a two-part key. Kept apart rather than generalised
/// because the driver differs in the one way that matters: a group is found by a row id and a
/// pair by two of them, and a lookup that took "some rows" would be a lookup nobody could read.
pub(super) fn paired(
    keys: &[usize],
    cells: &[Cell],
    having: Option<&Having>,
    order: Option<GroupOrder>,
    cut: Cut,
    values: &[Value],
) -> Vec<Row> {
    let of_each: Vec<&[Pair]> =
        keys.iter().filter_map(|i| values.get(*i)).map(|v| v.as_pairs().unwrap_or(&[])).collect();

    // Every pair any of this shape's plans produced, in key order. Several plans arise the same
    // way they do for a grouping - one aggregate apiece - and they describe the same pairs
    // unless a `FILTER` narrowed one of them.
    let mut rows: Vec<(RowId, RowId)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for pairs in &of_each {
        for p in *pairs {
            if seen.insert((p.left.row, p.right.row)) {
                rows.push((p.left.row, p.right.row));
            }
        }
    }
    rows.sort_by_key(|r| pair_order(&of_each, *r));

    let number = |of: Of, row: (RowId, RowId)| -> Option<Num> {
        match of {
            Of::Group { plan, absent } => {
                let found = values
                    .get(plan)?
                    .as_pairs()?
                    .iter()
                    .find(|p| (p.left.row, p.right.row) == row)
                    .and_then(|p| scalar_num(&p.right.value));
                match found {
                    Some(n) => Some(n),
                    None => match absent {
                        Absent::Zero => Some(Num::Int(0)),
                        Absent::Null => None,
                    },
                }
            }
            _ => None,
        }
    };

    let mut kept: Vec<(RowId, RowId)> = rows
        .into_iter()
        .filter(|row| match having {
            Some(h) => h.holds(&|of| int_of(number(of, *row))),
            None => true,
        })
        .collect();

    if let Some(o) = order {
        kept.sort_by(|a, b| {
            let by_key = pair_order(&of_each, *a).cmp(&pair_order(&of_each, *b));
            match o.by {
                OrderBy::Key => {
                    if o.desc {
                        by_key.reverse()
                    } else {
                        by_key
                    }
                }
                OrderBy::Value { of } => {
                    cmp_num(number(of, *a), number(of, *b), o.desc).then(by_key)
                }
            }
        });
    }

    let ties = |a: &(RowId, RowId), b: &(RowId, RowId)| match order {
        None => false,
        Some(o) => match o.by {
            OrderBy::Key => pair_order(&of_each, *a) == pair_order(&of_each, *b),
            OrderBy::Value { of } => number(of, *a) == number(of, *b),
        },
    };

    cut_rows(kept, cut, ties)
        .into_iter()
        .map(|row| {
            let found =
                of_each.iter().find_map(|ps| ps.iter().find(|p| (p.left.row, p.right.row) == row));
            cells
                .iter()
                .map(|c| match c.of {
                    Of::Key => {
                        found.and_then(|p| p.left.key.as_deref()).map_or(Datum::Null, Datum::text)
                    }
                    Of::RightKey => {
                        found.and_then(|p| p.right.key.as_deref()).map_or(Datum::Null, Datum::text)
                    }
                    of => Datum::num(number(of, row), &c.units),
                })
                .collect()
        })
        .collect()
}

/// A pair's place in key order: by the left key, then the right, unnamed after named.
fn pair_order<'a>(
    of_each: &[&'a [Pair]],
    row: (RowId, RowId),
) -> (bool, Option<&'a str>, RowId, bool, Option<&'a str>, RowId) {
    let found = of_each.iter().find_map(|ps| ps.iter().find(|p| (p.left.row, p.right.row) == row));
    let (l, r) = match found {
        Some(p) => (p.left.key.as_deref(), p.right.key.as_deref()),
        None => (None, None),
    };
    (l.is_none(), l, row.0, r.is_none(), r, row.1)
}

/// The rows of a join: one per key every side holds, or one folding all of them.
///
/// **The join is on the key's string.** Row ids are per `(table, field)` and two tables have no
/// reason to agree about them, so the strings are what pair up - and they are complete by the
/// time they get here, because a node is told the string of every row it is given records under.
/// A group with no string cannot be paired and is left out rather than fused with every other
/// unnamed one.
#[allow(clippy::too_many_arguments)]
pub(super) fn joined(
    sides: &[JoinSide],
    cells: &[Cell],
    per_key: bool,
    having: Option<&Having>,
    order: Option<GroupOrder>,
    cut: Cut,
    values: &[Value],
) -> Vec<Row> {
    let read = Reader::of(sides.to_vec(), cells, having, order, values);

    // The points of the join, in order. An inner join is the intersection: a key any one side is
    // missing pairs with nothing, which is no rows rather than a row of nulls.
    let mut space = read.row_space();

    if !per_key {
        return vec![cells
            .iter()
            .map(|c| Datum::num(read.cell(c.of, &space, None), &c.units))
            .collect()];
    }

    // `HAVING`, then `ORDER BY`, then `OFFSET`, then `LIMIT`, which is the order SQL specifies
    // and the order the grouped shape applies them in.
    let all = space.clone();
    space.retain(|at| match having {
        Some(h) => h.holds(&|of| int_of(read.cell(of, &all, Some(*at)))),
        None => true,
    });
    if let Some(o) = order {
        space.sort_by(|a, b| {
            let by_key = a.cmp(b);
            match o.by {
                OrderBy::Key => {
                    if o.desc {
                        by_key.reverse()
                    } else {
                        by_key
                    }
                }
                OrderBy::Value { of } => {
                    cmp_num(read.cell(of, &all, Some(*a)), read.cell(of, &all, Some(*b)), o.desc)
                        .then(by_key)
                }
            }
        });
    }

    let ties = |a: &Point<'_>, b: &Point<'_>| match order {
        None => false,
        Some(o) => match o.by {
            OrderBy::Key => a == b,
            OrderBy::Value { of } => read.cell(of, &all, Some(*a)) == read.cell(of, &all, Some(*b)),
        },
    };

    cut_rows(space, cut, ties)
        .into_iter()
        .map(|at| {
            cells
                .iter()
                .map(|c| match c.of {
                    Of::Key => at.axis(0).map_or(Datum::Null, Datum::text),
                    of => Datum::num(read.cell(of, &all, Some(at)), &c.units),
                })
                .collect()
        })
        .collect()
}

/// One row of the join's key space: the value of each axis the join pairs on.
///
/// An enum of fixed widths rather than a list, because the width is a fact about the join rather
/// than about the row - and because a list would allocate once per point in a walk that already
/// costs points times cells. Derived `Ord` is the lexicographic order the cut and `ORDER BY` on
/// the key already assume the points arrive in.
///
/// One axis is every join this surface lowers. The shape is an enum so that the two joins it
/// does not lower yet can arrive as variants rather than as a second walk: a cross join has no
/// axis at all and pairs at a single point, and a composite or chained key has two.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Point<'a> {
    /// One shared key.
    One(&'a str),
}

impl<'a> Point<'a> {
    /// The key at one axis, or `None` where this point has no such axis.
    fn axis(self, n: u8) -> Option<&'a str> {
        match (self, n) {
            (Self::One(k), 0) => Some(k),
            (Self::One(_), _) => None,
        }
    }
}

/// One plan's groups, by the key each was interned from.
type ByKey<'a> = std::collections::BTreeMap<&'a str, &'a Group>;

/// Every plan a join's shape reads, indexed by key rather than walked per lookup.
///
/// A join asks the same question of the same plan once per key and once per cell, and a plan's
/// groups arrive as a vector - so a scan per lookup costs the product of three lengths. Built
/// once, over the plans the shape actually names: with `UNION ALL` the statement's list holds
/// other branches' answers too, and indexing those would be work for rows this branch is not
/// about.
struct Reader<'a> {
    /// The join's tables, in the order the shape named them.
    sides: Vec<JoinSide>,
    by_plan: std::collections::BTreeMap<usize, ByKey<'a>>,
}

impl<'a> Reader<'a> {
    fn of(
        sides: Vec<JoinSide>,
        cells: &[Cell],
        having: Option<&Having>,
        order: Option<GroupOrder>,
        values: &'a [Value],
    ) -> Self {
        let mut wanted: Vec<usize> = sides.iter().flat_map(|s| s.keyed.plans()).collect();
        for c in cells {
            wanted.extend(c.of.plans());
        }
        if let Some(h) = having {
            wanted.extend(h.plans());
        }
        if let Some(GroupOrder { by: OrderBy::Value { of }, .. }) = order {
            wanted.extend(of.plans());
        }
        // A group with no string cannot be paired and is left out rather than fused with every
        // other unnamed one.
        let by_plan = wanted
            .into_iter()
            .map(|p| {
                let by_key: ByKey<'a> = values
                    .get(p)
                    .and_then(Value::as_groups)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|g| g.key.as_deref().map(|k| (k, g)))
                    .collect();
                (p, by_key)
            })
            .collect();
        Self { sides, by_plan }
    }

    /// The points of the join: the keys every side holds, in key order.
    ///
    /// A `BTreeMap`'s keys are already sorted, so the intersection comes out sorted too - which
    /// the ordering and the cut both rely on.
    fn row_space(&self) -> Vec<Point<'a>> {
        let sides: Vec<&ByKey<'a>> =
            self.sides.iter().filter_map(|s| self.by_plan.get(&s.keyed.plan())).collect();
        let Some((first, rest)) = sides.split_first() else { return Vec::new() };
        first
            .keys()
            .copied()
            .filter(|k| rest.iter().all(|m| m.contains_key(k)))
            .map(Point::One)
            .collect()
    }

    /// One plan's number at one point, or `None` where it said nothing about it.
    fn num(&self, plan: usize, at: Point<'_>) -> Option<Num> {
        let key = at.axis(0)?;
        scalar_num(&self.by_plan.get(&plan)?.get(key)?.value)
    }

    /// What one side of the join contributes at one point.
    ///
    /// The one function every kind of side goes through: the number it multiplies into the
    /// product, or `None` where the side holds nothing there. Today every side is keyed on the
    /// one axis, so this is a lookup - which is the point of naming it rather than inlining it.
    fn side_num(&self, side: usize, at: Point<'_>) -> Option<Num> {
        self.num(self.sides.get(side)?.keyed.plan(), at)
    }

    /// One number a join's cell holds: at one point, or folded over the whole space.
    ///
    /// `space` is every point of the join, which the folded form walks and `count(DISTINCT k)`
    /// counts.
    fn cell(&self, of: Of, space: &[Point<'a>], at: Option<Point<'_>>) -> Option<Num> {
        match of {
            Of::Key => None,
            Of::SharedKeys => Some(Num::Int(space.len() as i128)),
            Of::Paired { plan, side, how } => match at {
                Some(at) => self.pair(plan, side, at, how),
                // Folded over every point of the join: a product sums, because that is how many
                // pairs there are altogether; an extreme takes the extreme of the extremes.
                None => space.iter().filter_map(|at| self.pair(plan, side, *at, how)).fold(
                    None,
                    |acc: Option<Num>, n| {
                        Some(match (acc, how) {
                            (None, _) => n,
                            (Some(a), Pairing::Product) => Num::Int(as_i128(a) + as_i128(n)),
                            (Some(a), Pairing::Least) if as_i128(n) < as_i128(a) => n,
                            (Some(a), Pairing::Greatest) if as_i128(n) > as_i128(a) => n,
                            (Some(a), _) => a,
                        })
                    },
                ),
            },
            // **Folded, then divided** - which is why this is not two cells and a division.
            // Both halves are scaled by the same per-key product, and that product is inside
            // both sums rather than outside the fraction, so it cancels nowhere: the mean of
            // the per-key means is a different number from the mean over the join.
            Of::PairedRatio { top, bottom, side } => {
                let sum = |plan| match at {
                    Some(at) => self.pair(plan, side, at, Pairing::Product),
                    None => space
                        .iter()
                        .filter_map(|at| self.pair(plan, side, *at, Pairing::Product))
                        .fold(None, |acc: Option<Num>, n| {
                            Some(Num::Int(acc.map_or(0, as_i128) + as_i128(n)))
                        }),
                };
                let over = as_f64(sum(bottom)?);
                // No records is no average, which is `null` rather than a division by zero -
                // the same answer `min` gives over nothing, for the same reason.
                if over == 0.0 {
                    return None;
                }
                Some(Num::Real(as_f64(sum(top)?) / over))
            }
            // No other cell is reachable through a shape the lowering produces for a join.
            _ => None,
        }
    }

    /// One point's per-side numbers, made into one: this side's, and every other side's.
    ///
    /// `None` when the plan carrying the number said nothing about the point, which for a point
    /// in the join means it had no records under it - a `min` over nothing rather than a zero.
    fn pair(&self, plan: usize, side: usize, at: Point<'_>, how: Pairing) -> Option<Num> {
        // Only a shape nobody lowered can name a side the join does not have.
        if side >= self.sides.len() {
            return None;
        }
        let mine = self.num(plan, at)?;
        match how {
            // Every record on one side pairs with every record on every other, at this point.
            // Checked, because sixteen sides of counts is a product `i128` need not hold - and
            // `None` is the answer every caller here already reads as "no number".
            Pairing::Product => {
                let mut n = as_i128(mine);
                for i in 0..self.sides.len() {
                    if i != side {
                        n = n.checked_mul(as_i128(self.side_num(i, at)?))?;
                    }
                }
                Some(Num::Int(n))
            }
            // The other sides decide only whether the point is in the join. Repeating a value
            // does not make it larger or smaller, so this side's number stands as it is - but
            // it stands only where there is something to pair with. **Asked rather than assumed
            // from the point being in the space**: an intersection guarantees it and a cross
            // join or an outer one does not, and an extreme over no pairs is `null`.
            Pairing::Least | Pairing::Greatest => {
                for i in 0..self.sides.len() {
                    if i != side {
                        self.side_num(i, at)?;
                    }
                }
                Some(mine)
            }
        }
    }
}

/// Applies `OFFSET`, then `LIMIT`, then `WITH TIES` to an ordered list.
///
/// The order is the one SQL specifies and the only one that reads right: an offset into a list
/// that has not been cut, then the cut, then whatever the ordering cannot tell apart from the
/// last row inside it. `ties_with` is given the last kept row and a candidate after it, because
/// what counts as a tie depends on which half the ordering sorted on.
fn cut_rows<T>(rows: Vec<T>, cut: Cut, ties_with: impl Fn(&T, &T) -> bool) -> Vec<T> {
    let mut out: Vec<T> = rows.into_iter().skip(cut.offset.unwrap_or(0)).collect();
    let Some(limit) = cut.limit else { return out };
    if out.len() <= limit {
        return out;
    }
    // `LIMIT 0` keeps nothing, and there is no last row for anything to tie with.
    if !cut.ties || limit == 0 {
        out.truncate(limit);
        return out;
    }
    let mut end = limit;
    while end < out.len() && ties_with(&out[limit - 1], &out[end]) {
        end += 1;
    }
    out.truncate(end);
    out
}

fn as_i128(n: Num) -> i128 {
    match n {
        Num::Int(v) => v,
        Num::Real(v) => v as i128,
    }
}

/// The first plan that answered about this group, for the key it carries.
///
/// Any of them will do: a row id means the same value everywhere, and the key is the string
/// that row was interned from.
fn find<'a>(of_each: &[&'a [Group]], row: RowId) -> Option<&'a Group> {
    of_each.iter().find_map(|groups| groups.iter().find(|g| g.row == row))
}

fn sort_rows(rows: &mut [RowId], order: GroupOrder, of_each: &[&[Group]], values: &[Value]) {
    rows.sort_by(|a, b| {
        // **The tie-break is always the key, ascending**, whichever half is being sorted on.
        // Groups arrive from the executor in key order, but a sort is not required to be stable
        // and the merge across nodes is not either, so leaving ties to fall where they land
        // would make the same data answer differently depending on which node replied first.
        let by_key = key_order(of_each, *a).cmp(&key_order(of_each, *b));
        match order.by {
            OrderBy::Key => {
                if order.desc {
                    by_key.reverse()
                } else {
                    by_key
                }
            }
            OrderBy::Value { of } => {
                let (x, y) = (number(of, values, Some(*a)), number(of, values, Some(*b)));
                cmp_num(x, y, order.desc).then(by_key)
            }
        }
    });
}

/// A group's sort position by key: named groups in key order, unnamed ones after them by row.
///
/// The same rule `big_exec::sort_by_key` applies, and it has to be the same one: an ordering
/// that disagreed with the order groups arrive in would reshuffle an answer that was already
/// correct. The leading flag is what puts unnamed groups last - `Option` orders `None` first,
/// and this order is `Some` first.
fn key_order<'a>(of_each: &[&'a [Group]], row: RowId) -> (bool, Option<&'a str>, RowId) {
    let key = find(of_each, row).and_then(|g| g.key.as_deref());
    (key.is_none(), key, row)
}
