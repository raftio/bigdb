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

//! The analytical comparison: `big` against the engines that sell into the same market.
//!
//! Deliberately a second trait rather than more methods on [`crate::Engine`]. The storage
//! comparison next door enforces three rules - same durability, same question, same index - and
//! the engines here cannot all keep the first of them. DataFusion has no transactions to make
//! durable; ClickHouse answers over a socket, so every number it returns carries a round trip
//! that no in-process engine pays. Folding them into one table would mean
//! either dropping the rules or pretending they still held.
//!
//! So: two tables, one workload, one ground truth. [`Locality`] is printed in the report
//! against every row, and the two localities are never ranked against each other.

use crate::wide::{self, WideRecord};
use std::path::Path;
use std::time::{Duration, Instant};

/// Where the engine runs, which decides which table it belongs in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Locality {
    /// Linked into the benchmark process. Comparable with `big` directly.
    InProcess,
    /// A server the harness talks to over HTTP. Every timing includes a round trip, and the
    /// report prints the round trip separately so a reader can subtract it.
    OverHttp,
}

impl Locality {
    pub fn label(self) -> &'static str {
        match self {
            Self::InProcess => "in-process",
            Self::OverHttp => "over HTTP",
        }
    }
}

/// Whether an engine can answer a question at all, so the report can print `n/a` with a reason
/// instead of a blank or a zero.
///
/// DataFusion over Parquet is the case this exists for: it is a query engine over immutable
/// files and has no transaction to measure. Saying so is a result. Leaving the cell empty is
/// not.
#[derive(Clone, Debug)]
pub enum Answer<T> {
    Given(T),
    NotApplicable(&'static str),
}

impl<T> Answer<T> {
    pub fn given(&self) -> Option<&T> {
        match self {
            Self::Given(v) => Some(v),
            Self::NotApplicable(_) => None,
        }
    }
}

/// The six questions, and the corpus they are asked about.
///
/// Fixed on the harness rather than passed per call so that every engine is demonstrably asked
/// the same thing: the parameters are chosen once, printed once, and reused.
#[derive(Clone, Copy, Debug)]
pub struct Questions {
    /// The threshold for `count_ge`, `sum_where`, `distinct` and the intersection.
    pub k: u64,
    /// The country the intersection narrows to.
    pub country: u32,
    /// The `active` value the intersection narrows to.
    pub active: bool,
    /// How many groups `top_n` asks for.
    pub n: usize,
}

impl Default for Questions {
    /// `k` at three quarters of the ceiling selects a quarter of the corpus, which is the same
    /// selectivity the storage comparison's `count_ge` row uses - so the two tables' shared
    /// column means the same thing.
    fn default() -> Self {
        Self { k: wide::VALUE_CEILING / 4 * 3, country: 7, active: true, n: 10 }
    }
}

/// What every engine in the analytical comparison must be able to answer.
///
/// Six questions, each one a shape a bit-sliced index is claimed to be good at, and each one
/// something a column store answers natively too. Nothing here is expressible in only one of
/// the two idioms: `big` answers through its own query language and the SQL engines through
/// SQL, and neither side is made to write a loop the other does not have to.
pub trait Olap: Sized {
    fn name() -> &'static str;
    fn locality() -> Locality;

    /// Opens an engine in an empty directory. Engines that run as a server ignore it.
    fn open(dir: &Path) -> Self;

    /// Loads the whole corpus, by whatever bulk path the engine offers its users.
    ///
    /// Bulk rather than batched, because batch size is the storage comparison's axis and
    /// re-measuring it here would produce a worse version of a table that already exists. What
    /// this table measures is what the engine does with the data once it holds it.
    fn load(&mut self, records: &[WideRecord]);

    /// `Count(Row(amount >= k))` / `SELECT count(*) … WHERE amount >= k`.
    fn count_ge(&self, k: u64) -> u64;

    /// A three-term intersection: one low-cardinality key, one boolean, one range.
    fn intersect_count(&self, country: u32, active: bool, k: u64) -> u64;

    /// `Sum(Row(amount >= k), field=amount)`.
    fn sum_where(&self, k: u64) -> u128;

    /// Every category with its count, ordered by category.
    fn group_by_count(&self) -> Vec<(u32, u64)>;

    /// The `n` largest categories by count, ordered by count descending.
    fn top_n(&self, n: usize) -> Vec<(u32, u64)>;

    /// `COUNT(DISTINCT category) WHERE amount >= k`.
    fn distinct(&self, k: u64) -> u64;

    /// Flushes whatever the engine defers, so a size taken afterwards is honest.
    fn checkpoint(&mut self);

    /// Bytes on disk after `checkpoint`, where the harness owns the directory.
    ///
    /// `NotApplicable` for a server: its data directory holds system tables, logs and its own
    /// metadata, and reporting that as the cost of the corpus would be a made-up number.
    fn disk_bytes(&self) -> Answer<u64>;

    /// The cost of asking a server nothing, which is the floor under every other timing it
    /// reports. `None` for anything in-process, which has no floor to subtract.
    fn round_trip(&self) -> Option<Duration> {
        None
    }
}

/// Every answer one engine gave, with the time each took.
#[derive(Clone, Debug)]
pub struct Measured {
    pub name: &'static str,
    pub locality: Locality,
    pub load: Duration,
    pub count_ge: (u64, Duration),
    pub intersect: (u64, Duration),
    pub sum_where: (u128, Duration),
    pub group_by: (usize, Duration),
    pub top_n: (usize, Duration),
    pub distinct: (u64, Duration),
    pub disk_bytes: Answer<u64>,
    pub round_trip: Option<Duration>,
}

/// Three rather than more, for the same reason the storage report gives: a run is seconds, not
/// microseconds, and a median of three discards one descheduled run and nothing more.
pub const REPEATS: usize = 3;

fn median(mut xs: Vec<Duration>) -> Duration {
    xs.sort_unstable();
    xs[xs.len() / 2]
}

/// Times `f` `REPEATS` times and returns its answer with the median duration.
///
/// The answer from the last run rather than the first: if an engine were nondeterministic
/// across repeats, the checker below would catch it whichever one was kept, and keeping the
/// last means the returned answer came from a warm engine like the timing did.
fn timed<T>(mut f: impl FnMut() -> T) -> (T, Duration) {
    let mut answer = None;
    let mut times = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let t0 = Instant::now();
        let a = f();
        times.push(t0.elapsed());
        answer = Some(a);
    }
    (answer.expect("REPEATS is never zero"), median(times))
}

/// Runs every question against one engine and checks each answer against ground truth before
/// keeping the timing.
///
/// **The check is not optional and does not warn.** The storage harness's rule is that a
/// benchmark measuring a wrong answer measures nothing, and this is that rule applied to six
/// harder questions. An engine that disagrees with `wide` fails here, loudly, with both numbers
/// printed - because the likeliest cause is a mistake in this harness's adapter, and a warning
/// would let it be read as a result.
pub fn measure<E: Olap>(dir: &Path, records: &[WideRecord], q: Questions) -> Measured {
    let mut engine = E::open(dir);

    let t0 = Instant::now();
    engine.load(records);
    let load = t0.elapsed();
    engine.checkpoint();

    let (count_ge, t_count) = timed(|| engine.count_ge(q.k));
    check(E::name(), "count_ge", count_ge, wide::expected_count_ge(records, q.k));

    let (intersect, t_intersect) = timed(|| engine.intersect_count(q.country, q.active, q.k));
    check(
        E::name(),
        "intersect",
        intersect,
        wide::expected_intersect_count(records, q.country, q.active, q.k),
    );

    let (sum, t_sum) = timed(|| engine.sum_where(q.k));
    check(E::name(), "sum_where", sum, wide::expected_sum_where(records, q.k));

    let (groups, t_group) = timed(|| engine.group_by_count());
    check(E::name(), "group_by", groups.clone(), wide::expected_group_counts(records));

    let (top, t_top) = timed(|| engine.top_n(q.n));
    check(E::name(), "top_n", top.clone(), wide::expected_top_n(records, q.n));

    let (distinct, t_distinct) = timed(|| engine.distinct(q.k));
    check(E::name(), "distinct", distinct, wide::expected_distinct(records, q.k));

    Measured {
        name: E::name(),
        locality: E::locality(),
        load,
        count_ge: (count_ge, t_count),
        intersect: (intersect, t_intersect),
        sum_where: (sum, t_sum),
        group_by: (groups.len(), t_group),
        top_n: (top.len(), t_top),
        distinct: (distinct, t_distinct),
        disk_bytes: engine.disk_bytes(),
        round_trip: engine.round_trip(),
    }
}

fn check<T: PartialEq + std::fmt::Debug>(engine: &str, question: &str, got: T, want: T) {
    assert!(
        got == want,
        "{engine} answered {question} wrongly.\n  got:  {got:?}\n  want: {want:?}\n\
         The ground truth in `wide` is computed without any engine, so this is either a bug in \
         the {engine} adapter or a bug in {engine}. Either way the timing beside it would be \
         measuring the wrong query."
    );
}
