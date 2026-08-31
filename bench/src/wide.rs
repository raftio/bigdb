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

//! The wide workload, and the ground truth for every question asked of it.
//!
//! The storage comparison next door needs one integer field, because the question it asks is
//! about the storage. An analytical comparison cannot: `GroupBy`, `TopN` and `Distinct` have no
//! meaning over a table with one column, and a benchmark that asked only `count_ge` would be
//! the storage comparison again under a different heading.
//!
//! Every answer in this module is computed with iterators over a `Vec`, and no engine is
//! involved in producing any of them. That is the point: an engine that disagrees with this
//! file has failed the benchmark rather than won it.

use crate::{Layout, SHARD_WIDTH};

/// Values stay under 2^20, which is the bit depth `big` is told to allocate.
pub const VALUE_CEILING: u64 = 1 << 20;

/// Distinct values of `category`, the field `GroupBy`, `TopN` and `Distinct` are asked about.
///
/// 256 rather than something larger because the quotas below must be strictly decreasing and
/// still sum to the corpus size, which needs `n >= C(C+1)/2` - 32,896 here. It is also the
/// cardinality at which a group-by is interesting rather than degenerate: large enough that the
/// answer is a table, small enough that the table is readable.
pub const CATEGORIES: u32 = 256;

/// Distinct values of `country`. Low cardinality on purpose: this is the field an intersection
/// narrows with, and a predicate matching one row in a million measures nothing.
pub const COUNTRIES: u32 = 20;

/// One record across every column the analytical comparison asks about.
///
/// Four columns, four shapes: a wide integer for range and sum, a mid-cardinality key for
/// grouping, a low-cardinality key for intersecting, and a boolean. Between them they cover
/// what a bit-sliced index is claimed to be good at, which is the claim under test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WideRecord {
    pub id: u64,
    /// 0..`VALUE_CEILING`, not monotonic in the record id.
    pub amount: u64,
    /// 0..`CATEGORIES`, with a strictly decreasing frequency distribution. See
    /// [`category_quotas`] for why the frequencies are not allowed to tie.
    pub category: u32,
    /// 0..`COUNTRIES`, near-uniform.
    pub country: u32,
    pub active: bool,
}

impl WideRecord {
    /// The key `big` interns and the string SQL engines store, so both sides of the comparison
    /// hold the same value rather than one holding an integer and the other a string.
    pub fn category_key(category: u32) -> String {
        format!("c{category:03}")
    }

    pub fn country_key(country: u32) -> String {
        format!("n{country:02}")
    }
}

/// How many records each category gets, strictly decreasing and summing to `n`.
///
/// **Strictly decreasing is a correctness requirement, not an aesthetic one.** `TopN` orders by
/// count, and every engine here breaks a tie differently - `big` breaks on the interned key,
/// SQL engines break on whatever the executor happened to produce first. A workload whose two
/// largest groups are the same size would therefore have several correct answers, and a
/// benchmark that cannot say which answer is right cannot say an engine got it wrong. Distinct
/// frequencies remove the ambiguity at the source instead of papering over it in the checker.
///
/// Panics rather than degrading if `n` is too small to give 256 categories distinct quotas: a
/// silently tied distribution is exactly the failure this function exists to prevent.
pub fn category_quotas(n: u64) -> Vec<u64> {
    let c = u64::from(CATEGORIES);
    let triangle = c * (c + 1) / 2;
    assert!(
        n >= triangle,
        "the wide workload needs at least {triangle} records to give {CATEGORIES} categories \
         strictly distinct frequencies; got {n}"
    );

    // Start from c, c-1, ... 1 - every adjacent gap exactly one - then spread what is left over
    // evenly and hand the remainder to a prefix. Adding one to a prefix widens the gap at the
    // boundary from one to two and leaves every other gap alone, so the sequence stays strictly
    // decreasing however the remainder falls.
    let spare = n - triangle;
    let each = spare / c;
    let remainder = spare % c;
    (0..c).map(|i| (c - i) + each + u64::from(i < remainder)).collect()
}

/// A deterministic 64-bit mixer, so the shuffle below needs no RNG crate and gives the same
/// permutation on every machine. `splitmix64`, unchanged.
fn mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The category of every record, in record order: each category repeated its quota times, then
/// shuffled.
///
/// **The shuffle is what makes the comparison honest.** Handing out categories in blocks would
/// give every group one contiguous run of record ids, which is simultaneously the best case for
/// a bitmap - one run per container - and the best case for a clustered column store, so
/// neither engine would be measured on anything but the harness's own convenience. An earlier
/// version tried to scatter with a stride coprime to `n`; it is a permutation, but consecutive
/// records land near a handful of anchor points rather than anywhere, and the first hundred
/// records covered five categories instead of most of a hundred. A full shuffle has no such
/// structure to accidentally rely on.
fn shuffled_categories(n: u64) -> Vec<u32> {
    let quotas = category_quotas(n);
    let mut out = Vec::with_capacity(n as usize);
    for (category, quota) in quotas.iter().enumerate() {
        out.extend(std::iter::repeat_n(category as u32, *quota as usize));
    }

    // Fisher-Yates, back to front, from a fixed seed. Deterministic across machines because
    // nothing here is wider than 64 bits or dependent on a library's idea of a shuffle.
    let mut state = 0x5EED_1234_5678_9ABC;
    for i in (1..out.len()).rev() {
        let j = (mix(&mut state) % (i as u64 + 1)) as usize;
        out.swap(i, j);
    }
    out
}

/// Deterministic wide workload. Same reasoning as [`crate::workload`]: a real RNG would make
/// runs incomparable across machines and buy nothing.
pub fn wide_workload(n: u64, layout: Layout) -> Vec<WideRecord> {
    let categories = shuffled_categories(n);

    (0..n)
        .map(|i| {
            let id = match layout {
                Layout::Dense => i,
                Layout::Sparse { shards } => (i % shards) * SHARD_WIDTH + i / shards,
            };
            // Knuth's multiplicative hash, unchanged from the storage workload so the
            // `count_ge` column means the same thing in both comparisons.
            let amount = i.wrapping_mul(2_654_435_761) % VALUE_CEILING;
            let category = categories[i as usize];
            // Different constants per column, so no two columns are functions of each other and
            // an intersection actually narrows.
            let country =
                ((i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) % u64::from(COUNTRIES)) as u32;
            let active = (i.wrapping_mul(0xD6E8_FEB8_6659_FD93) >> 33) & 1 == 0;
            WideRecord { id, amount, category, country, active }
        })
        .collect()
}

/// `Count(Row(amount >= k))`.
pub fn expected_count_ge(records: &[WideRecord], k: u64) -> u64 {
    records.iter().filter(|r| r.amount >= k).count() as u64
}

/// `Count(Intersect(Row(country=…), Row(active=true), Row(amount >= k)))`.
pub fn expected_intersect_count(records: &[WideRecord], country: u32, active: bool, k: u64) -> u64 {
    records.iter().filter(|r| r.country == country && r.active == active && r.amount >= k).count()
        as u64
}

/// `Sum(Row(amount >= k), field=amount)`.
///
/// `u128` because the total of a hundred thousand twenty-bit values fits in `u64` and the total
/// of two million does not stay comfortable there; the engines return their own widths and the
/// harness should not be the thing that overflows.
pub fn expected_sum_where(records: &[WideRecord], k: u64) -> u128 {
    records.iter().filter(|r| r.amount >= k).map(|r| u128::from(r.amount)).sum()
}

/// `GroupBy(All(), field=category, aggregate=Count)`, ordered by category.
pub fn expected_group_counts(records: &[WideRecord]) -> Vec<(u32, u64)> {
    let mut counts = vec![0u64; CATEGORIES as usize];
    for r in records {
        counts[r.category as usize] += 1;
    }
    counts.into_iter().enumerate().map(|(c, n)| (c as u32, n)).filter(|(_, n)| *n > 0).collect()
}

/// `TopN(All(), field=category, n=…)`, ordered by count descending.
///
/// No tie-break is specified because [`category_quotas`] guarantees there are no ties. If that
/// ever stops being true this function is where it will show up, as two runs disagreeing.
pub fn expected_top_n(records: &[WideRecord], n: usize) -> Vec<(u32, u64)> {
    let mut groups = expected_group_counts(records);
    groups.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    groups.truncate(n);
    groups
}

/// `Distinct(Row(amount >= k), field=category)`, reduced to how many categories survived.
///
/// The scalar rather than the list, because the list is what `expected_group_counts` already
/// checks and `COUNT(DISTINCT …)` is the shape every SQL engine here answers natively.
pub fn expected_distinct(records: &[WideRecord], k: u64) -> u64 {
    let mut seen = vec![false; CATEGORIES as usize];
    for r in records.iter().filter(|r| r.amount >= k) {
        seen[r.category as usize] = true;
    }
    seen.iter().filter(|s| **s).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: u64 = 100_000;

    #[test]
    fn quotas_sum_to_the_corpus_and_never_tie() {
        for n in [32_896u64, 100_000, 200_000, 1_000_000] {
            let q = category_quotas(n);
            assert_eq!(q.len(), CATEGORIES as usize);
            assert_eq!(q.iter().sum::<u64>(), n, "quotas must account for every record");
            for w in q.windows(2) {
                assert!(w[0] > w[1], "quotas must strictly decrease, or TopN has no right answer");
            }
        }
    }

    #[test]
    #[should_panic(expected = "strictly distinct frequencies")]
    fn quotas_refuse_a_corpus_too_small_to_separate() {
        category_quotas(32_895);
    }

    #[test]
    fn the_workload_honours_its_quotas() {
        let records = wide_workload(N, Layout::Dense);
        assert_eq!(records.len() as u64, N);

        let quotas = category_quotas(N);
        let mut seen = vec![0u64; CATEGORIES as usize];
        for r in &records {
            seen[r.category as usize] += 1;
        }
        assert_eq!(
            seen, quotas,
            "generated frequencies must match the quotas they were built from"
        );
    }

    #[test]
    fn categories_are_scattered_rather_than_blocked() {
        let records = wide_workload(N, Layout::Dense);
        // The largest category holds ~600 records. If they were assigned in blocks the first
        // 600 ids would all share one category; scattered, the first hundred ids should touch
        // most of a hundred different categories.
        let distinct: std::collections::HashSet<u32> =
            records[..100].iter().map(|r| r.category).collect();
        assert!(distinct.len() > 80, "categories look blocked, not scattered: {}", distinct.len());
    }

    #[test]
    fn columns_are_not_functions_of_each_other() {
        let records = wide_workload(N, Layout::Dense);
        // Every country must see both boolean values and a spread of amounts, or an
        // intersection would be answerable from one of its terms alone.
        for country in 0..COUNTRIES {
            let of_country: Vec<_> = records.iter().filter(|r| r.country == country).collect();
            assert!(!of_country.is_empty(), "country {country} is empty");
            assert!(of_country.iter().any(|r| r.active), "country {country} has no active rows");
            assert!(of_country.iter().any(|r| !r.active), "country {country} is entirely active");
        }
    }

    #[test]
    fn amounts_are_not_monotonic_in_the_record_id() {
        let records = wide_workload(N, Layout::Dense);
        let descents = records.windows(2).filter(|w| w[1].amount < w[0].amount).count();
        assert!(descents > N as usize / 4, "amounts look sorted; a prefix could answer a range");
    }

    #[test]
    fn top_n_is_unambiguous() {
        let records = wide_workload(N, Layout::Dense);
        let top = expected_top_n(&records, 10);
        assert_eq!(top.len(), 10);
        for w in top.windows(2) {
            assert!(w[0].1 > w[1].1, "two groups tied inside TopN: {:?}", w);
        }
        // The quota vector hands the largest quota to category 0 and descends from there.
        assert_eq!(top[0].0, 0);
    }

    #[test]
    fn ground_truth_agrees_with_itself() {
        let records = wide_workload(N, Layout::Dense);
        let k = VALUE_CEILING / 4 * 3;

        let ge = expected_count_ge(&records, k);
        assert!(ge > 0 && ge < N, "the predicate must select some records and not all: {ge}");

        // An intersection can only ever be a subset of any one of its terms.
        let intersected: u64 =
            (0..COUNTRIES).map(|c| expected_intersect_count(&records, c, true, k)).sum();
        let active_ge = records.iter().filter(|r| r.active && r.amount >= k).count() as u64;
        assert_eq!(intersected, active_ge, "the intersection must partition by country");

        // Group counts must account for every record, and distinct must not exceed the number
        // of groups that exist.
        let groups = expected_group_counts(&records);
        assert_eq!(groups.iter().map(|(_, n)| n).sum::<u64>(), N);
        assert!(expected_distinct(&records, k) <= groups.len() as u64);

        // The sum over a predicate must sit between the smallest and largest it could be.
        let sum = expected_sum_where(&records, k);
        assert!(sum >= u128::from(ge) * u128::from(k));
        assert!(sum < u128::from(ge) * u128::from(VALUE_CEILING));
    }

    #[test]
    fn sparse_layouts_hold_the_same_facts_in_different_places() {
        let dense = wide_workload(N, Layout::Dense);
        let sparse = wide_workload(N, Layout::Sparse { shards: 8 });
        // The ids move; nothing else does. Every question in this module is about values, so
        // both layouts must give identical answers to all of them.
        assert_eq!(expected_group_counts(&dense), expected_group_counts(&sparse));
        assert_eq!(expected_top_n(&dense, 10), expected_top_n(&sparse, 10));
        let k = VALUE_CEILING / 2;
        assert_eq!(expected_count_ge(&dense, k), expected_count_ge(&sparse, k));
        assert_eq!(expected_sum_where(&dense, k), expected_sum_where(&sparse, k));
        assert_ne!(dense[1].id, sparse[1].id, "the layouts must actually differ");
    }
}
