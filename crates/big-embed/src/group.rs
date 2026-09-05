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

//! Many writes, one commit.
//!
//! A commit costs two fsyncs, a full catalog clone on the way in and a full catalog encode on
//! the way out. None of that is per fact, so a server answering ten concurrent imports pays it
//! ten times for no reason: the store allows one writer, so those ten were going to be
//! serialised anyway. This makes them share.
//!
//! **The trick is where the group is collected, and it is not a timer.** The first writer to
//! arrive becomes the leader and calls [`Db::write`], which blocks on the store's write lock
//! exactly as it does today. It drains the queue *after* that call returns — so the window in
//! which followers accumulate is precisely the window the leader was blocked anyway. Under
//! contention the group forms for free; with no contention the queue is empty and nothing has
//! been added to the path but one relaxed load. A linger timer would buy grouping by making an
//! idle server slower; this buys it by making a busy one faster.
//!
//! **A bad batch must not take the group down with it.** Rollback is free — `WriteTxn`'s
//! destructor does nothing at all, because no page reached the disk — so a group that fails is
//! dropped and re-run by contiguous halves until the offender is alone. See
//! [`Group::isolate_range`].
//!
//! **Jobs own what they write, and that is not negotiable.** The leader is another thread, so
//! handing it facts borrowed from a request body would mean erasing a lifetime, which means
//! `unsafe`, which this crate denies at its root and the README sells as a property of
//! everything above `big-pager`. The cost is paid the cheap way instead: [`Batch`] interns each
//! distinct field name and key once and holds four-byte indices — the representation
//! `big_db::ingest` already measured — so a hundred thousand facts spread over forty field
//! names allocate forty times rather than a hundred thousand.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use big_db::{Db, DbWrite, RecordId, RowId};
use big_pager::{PagerMut, TxnId};

use crate::error::{ApiError, Result};
use crate::{Fact, KeyAssignment};

/// When a write is answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Ack {
    /// After the commit that carried it has been flushed.
    ///
    /// The promise every write has always made, and the default. A caller that does nothing
    /// differently gets exactly what it got before.
    #[default]
    Sync,
    /// As soon as the facts are held, with a later commit making them durable.
    ///
    /// **This is a different promise, not a faster one.** What has been acknowledged and not yet
    /// committed is lost if the process dies, and a batch the engine turns out to refuse is
    /// counted in [`GroupStats::async_failed`] rather than reported to whoever sent it - there
    /// is nobody left to report it to. Both are inherent to answering early; neither is a bug to
    /// be fixed later.
    Async,
}

/// What to do with an [`Ack::Async`] write when the buffer is full.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WhenFull {
    /// Answer it as though it had asked for [`Ack::Sync`]: park, help commit, return durable.
    ///
    /// Backpressure applied to the client that caused it, with no new error and no lost write.
    Block,
    /// Refuse it, so the client backs off instead of this process growing.
    Refuse,
}

/// What one write managed.
///
/// **This caller's own numbers, never the group's.** A batch of three facts that happened to
/// commit alongside nine hundred others is still a batch of three facts, and a caller told
/// otherwise would report nonsense to whoever asked it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Accepted {
    /// Facts written, or records removed — the same number the uncoalesced call returned.
    pub count: u64,
    /// The transaction that carried it, and `None` when the answer came before there was one.
    ///
    /// Correct for every job in a group, because that is what a group commit *is*.
    pub txn: Option<TxnId>,
}

/// How many jobs one commit may carry, and how many facts.
///
/// Both bound the **group**, never the queue: a leader takes up to the cap and leaves the rest
/// for the next round, so neither is ever a refusal. They exist for different failures.
/// `max_jobs` bounds the worst case of splitting a failed group, and one caller's tail latency;
/// `max_facts` bounds head-of-line blocking, which is a one-fact request waiting out the
/// million-fact request it happened to arrive behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupConfig {
    /// Off unless something turns it on. A node that changes how it commits without being asked
    /// is a node whose behaviour an operator cannot predict.
    pub enabled: bool,
    /// Jobs one commit may carry.
    pub max_jobs: usize,
    /// Facts one commit may carry, counted across every job in it. A single job larger than
    /// this still goes alone, or it would never go at all.
    pub max_facts: usize,
    /// Whether [`Ack::Async`] is allowed at all. Off unless something turns it on, and
    /// separately from [`GroupConfig::enabled`], because answering early is a change to what a
    /// write *promises* rather than to what it costs.
    pub async_writes: bool,
    /// Bytes of acknowledged-but-uncommitted work this may hold.
    ///
    /// **The only memory this feature actually owns.** A sync waiter is a parked caller holding
    /// its own batch, so the number of them is bounded by whatever bounds callers; async work
    /// is bounded by nothing but this. Measured in bytes rather than jobs or facts because jobs
    /// vary by four orders of magnitude and bytes is what a machine is sized in.
    pub max_async_bytes: usize,
    /// The longest an acknowledged write may wait for a commit.
    ///
    /// The ceiling on how stale an early answer can be, and the only thing here measured in
    /// time. Nothing else waits on a clock.
    pub linger: Duration,
    /// What a full buffer does to an [`Ack::Async`] write.
    pub when_full: WhenFull,
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_jobs: 64,
            max_facts: 1 << 20,
            async_writes: false,
            max_async_bytes: 64 << 20,
            // The same 200ms `contrib/big-message` lingers on the client side. One vocabulary
            // for one idea, rather than a second number to reason about.
            linger: Duration::from_millis(200),
            when_full: WhenFull::Block,
        }
    }
}

/// Counters, read at scrape time.
///
/// `jobs / commits` is the one number that says whether this is working: it is the average
/// group size, and it is `1.00` on a server with no write contention however the flags are set.
/// `isolations` is the one that says a client is making everybody else pay.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GroupStats {
    /// Transactions that reached the disk.
    pub commits: u64,
    /// Jobs those transactions carried.
    pub jobs: u64,
    /// Groups that failed and had to be split.
    pub isolations: u64,
    /// Transactions opened while splitting, the failed ones included.
    pub isolation_attempts: u64,
    /// Bytes of acknowledged work not yet committed. What is lost if this process dies now.
    pub async_held_bytes: u64,
    /// How long the oldest acknowledged-but-uncommitted write has been waiting, in seconds.
    ///
    /// **The number to alert on.** It is the durability lag, and it is the one thing an early
    /// answer costs that a caller cannot see for itself.
    pub async_oldest_seconds: f64,
    /// Writes refused because the buffer was full and the policy was to refuse.
    pub async_refused: u64,
    /// Acknowledged writes the engine then turned out to refuse.
    ///
    /// **Nobody was told.** The caller had already been answered, so this counter is the only
    /// place the failure appears. Any value above zero is data that was accepted and did not
    /// land, and it should be alerted on rather than watched.
    pub async_failed: u64,
}

/// One buffered write, owning what it writes.
///
/// Three variants rather than a closure, because the lifetime story here has to be reviewable:
/// three concrete shapes can be read once and understood; a `dyn Fn` cannot.
pub(crate) enum Work {
    /// [`crate::Api::import`].
    Import { table: String, batch: Batch },
    /// [`crate::Api::import_with_keys`]. The assignments land in the same transaction as the
    /// facts, exactly as they do uncoalesced.
    ImportWithKeys { table: String, keys: Vec<OwnedKey>, batch: Batch },
    /// [`crate::Api::delete`].
    Delete { table: String, records: Vec<RecordId> },
}

impl Work {
    /// What this job adds to a group, for `max_facts`.
    ///
    /// A delete counts its record ids: they are what the transaction has to touch, and removing
    /// a million records is not a small job for having named no fields.
    fn weight(&self) -> usize {
        match self {
            Self::Import { batch, .. } | Self::ImportWithKeys { batch, .. } => batch.ops.len(),
            Self::Delete { records, .. } => records.len(),
        }
    }

    /// Roughly what holding this costs, for `max_async_bytes`.
    ///
    /// Roughly on purpose: it counts what the job owns and not the allocator's overhead, so the
    /// true figure is larger. A ceiling on memory wants to be reached early rather than
    /// exactly, and an exact answer here would cost a walk per submission.
    fn bytes(&self) -> usize {
        let base = std::mem::size_of::<Self>();
        match self {
            Self::Import { table, batch } => base + table.len() + batch.bytes(),
            Self::ImportWithKeys { table, keys, batch } => {
                let k: usize = keys
                    .iter()
                    .map(|k| std::mem::size_of::<OwnedKey>() + k.field.len() + k.key.len())
                    .sum();
                base + table.len() + k + batch.bytes()
            }
            Self::Delete { table, records } => {
                base + table.len() + records.len() * std::mem::size_of::<RecordId>()
            }
        }
    }
}

/// A key assignment, owned.
pub(crate) struct OwnedKey {
    field: String,
    key: String,
    row: RowId,
}

impl OwnedKey {
    pub(crate) fn own(k: &KeyAssignment<'_>) -> Self {
        Self { field: k.field.to_string(), key: k.key.to_string(), row: k.row }
    }
}

/// Every distinct string a batch names, held once.
///
/// Lifted from `big_db::ingest::Pool`, which has the measurement written beside it: a key or a
/// field name is a short string from a small alphabet, repeated across every fact that mentions
/// it, and storing it inline allocated the same twenty bytes millions of times to hold a value
/// already in hand.
#[derive(Default)]
struct Pool {
    texts: Vec<String>,
    index: HashMap<String, u32>,
}

impl Pool {
    fn intern(&mut self, text: &str) -> u32 {
        if let Some(i) = self.index.get(text) {
            return *i;
        }
        let i = self.texts.len() as u32;
        self.texts.push(text.to_string());
        self.index.insert(text.to_string(), i);
        i
    }

    fn get(&self, i: u32) -> &str {
        &self.texts[i as usize]
    }
}

/// One fact, with its strings held by index.
struct Op {
    field: u32,
    record: RecordId,
    value: Value,
}

enum Value {
    Int(u64),
    Signed(i64),
    Float(u64),
    Bool(bool),
    Key(u32),
    Time { value: u32, unix_seconds: i64 },
}

/// Facts owned, with their strings interned.
///
/// [`Batch::facts`] hands them back as ordinary [`Fact`]s borrowing the pool, so everything
/// below this is the write path that was always there.
pub(crate) struct Batch {
    pool: Pool,
    ops: Vec<Op>,
}

impl Batch {
    /// Copies a borrowed batch, interning as it goes.
    pub(crate) fn own(facts: &[Fact<'_>]) -> Self {
        let mut pool = Pool::default();
        let mut ops = Vec::with_capacity(facts.len());
        for fact in facts {
            let field = pool.intern(fact.field());
            let (record, value) = match fact {
                Fact::Int { record, value, .. } => (*record, Value::Int(*value)),
                Fact::Signed { record, value, .. } => (*record, Value::Signed(*value)),
                Fact::Float { record, bits, .. } => (*record, Value::Float(*bits)),
                Fact::Bool { record, value, .. } => (*record, Value::Bool(*value)),
                Fact::Key { record, value, .. } => (*record, Value::Key(pool.intern(value))),
                Fact::Time { record, value, unix_seconds, .. } => (
                    *record,
                    Value::Time { value: pool.intern(value), unix_seconds: *unix_seconds },
                ),
            };
            ops.push(Op { field, record, value });
        }
        Self { pool, ops }
    }

    /// What the interned batch occupies, near enough for a ceiling.
    fn bytes(&self) -> usize {
        let texts: usize = self.pool.texts.iter().map(|t| t.len() + t.capacity() / 8).sum();
        // The index holds each string a second time.
        texts * 2 + self.ops.len() * std::mem::size_of::<Op>()
    }

    /// The facts, borrowing the pool.
    fn facts(&self) -> Vec<Fact<'_>> {
        self.ops
            .iter()
            .map(|op| {
                let field = self.pool.get(op.field);
                let record = op.record;
                match op.value {
                    Value::Int(value) => Fact::Int { field, record, value },
                    Value::Signed(value) => Fact::Signed { field, record, value },
                    Value::Float(bits) => Fact::Float { field, record, bits },
                    Value::Bool(value) => Fact::Bool { field, record, value },
                    Value::Key(value) => Fact::Key { field, record, value: self.pool.get(value) },
                    Value::Time { value, unix_seconds } => {
                        Fact::Time { field, record, value: self.pool.get(value), unix_seconds }
                    }
                }
            })
            .collect()
    }
}

/// What a submitter is waiting on.
enum Slot {
    /// Queued, and this is the work a leader will take.
    Waiting(Work),
    /// A leader holds the work and is committing it.
    Taken,
    /// Elected leader by the outgoing one; the work comes back with the job.
    Lead(Work),
    /// Answered.
    Done(Result<Accepted>),
}

/// One submitter's place in the queue.
struct Ticket {
    slot: Mutex<Slot>,
    wake: Condvar,
    /// What this job counts against `max_async_bytes`, and `0` when somebody is waiting on it.
    ///
    /// Non-zero is exactly what "nobody is coming back for this" means, so it is also how an
    /// error on this ticket is known to be one no caller will ever see.
    async_bytes: usize,
    /// When it was accepted, for the durability-lag gauge.
    at: Instant,
}

impl Ticket {
    fn new(work: Work) -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(Slot::Waiting(work)),
            wake: Condvar::new(),
            async_bytes: 0,
            at: Instant::now(),
        })
    }

    /// A ticket for a write that has already been answered.
    fn unattended(work: Work, bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(Slot::Waiting(work)),
            wake: Condvar::new(),
            async_bytes: bytes.max(1),
            at: Instant::now(),
        })
    }

    /// Answers this ticket and wakes whoever is on it.
    fn settle(&self, result: Result<Accepted>) {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Slot::Done(result);
        self.wake.notify_one();
    }

    /// Takes the answer back out, for a leader reading its own.
    fn answer(&self) -> Option<Result<Accepted>> {
        let mut slot = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        match std::mem::replace(&mut *slot, Slot::Taken) {
            Slot::Done(result) => Some(result),
            other => {
                *slot = other;
                None
            }
        }
    }
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Arc<Ticket>>,
    /// Whether somebody is already on their way to a commit.
    leading: bool,
    /// Bytes of acknowledged work waiting, and when the oldest of it was accepted.
    ///
    /// Held here rather than in an atomic because both change with the queue and must agree
    /// with it: a byte count that has been decremented for work still in `pending` is a ceiling
    /// that lets more in than it should.
    async_bytes: usize,
    oldest_async: Option<Instant>,
    /// Set by [`Group::stop`]. New submissions are refused; what is already here still commits.
    stopping: bool,
}

/// The queue, its election, and the counters.
///
/// Not generic over the pager: what it holds is owned work, and the database appears only as an
/// argument to [`Group::submit`]. That keeps one of these on an `Api` of any backing.
pub(crate) struct Group {
    /// Read before the mutex, so a server with this switched off pays one relaxed load and
    /// nothing else.
    enabled: AtomicBool,
    config: Mutex<GroupConfig>,
    queue: Mutex<Queue>,
    /// Notified when the first async job lands, so a flusher on an idle node never wakes at all
    /// rather than waking on a timer to find nothing.
    arrived: Condvar,
    commits: AtomicU64,
    jobs: AtomicU64,
    isolations: AtomicU64,
    isolation_attempts: AtomicU64,
    async_refused: AtomicU64,
    async_failed: AtomicU64,
}

impl Default for Group {
    fn default() -> Self {
        let config = GroupConfig::default();
        Self {
            enabled: AtomicBool::new(config.enabled),
            config: Mutex::new(config),
            queue: Mutex::new(Queue::default()),
            arrived: Condvar::new(),
            commits: AtomicU64::new(0),
            jobs: AtomicU64::new(0),
            isolations: AtomicU64::new(0),
            isolation_attempts: AtomicU64::new(0),
            async_refused: AtomicU64::new(0),
            async_failed: AtomicU64::new(0),
        }
    }
}

/// Every job a leader took, and the promise that each one is answered.
///
/// The destructor is the mechanism, not a tidy-up. A leader that panics between taking a
/// follower's work and answering it would otherwise leave that follower parked on its condvar
/// for the life of the process — and `big_http` catches panics per request, so a panicking
/// leader is something that happens rather than something that ends the program.
struct Taken<'g> {
    jobs: Vec<(Arc<Ticket>, Option<Work>)>,
    /// Where an error nobody will ever read gets counted. See [`GroupStats::async_failed`].
    async_failed: &'g AtomicU64,
}

impl Taken<'_> {
    /// Answers one job, counting it if the answer is a failure nobody is waiting for.
    fn settle(&self, i: usize, result: Result<Accepted>) {
        let (ticket, _) = &self.jobs[i];
        if result.is_err() && ticket.async_bytes > 0 {
            self.async_failed.fetch_add(1, Ordering::Relaxed);
        }
        ticket.settle(result);
    }
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        for (ticket, work) in self.jobs.drain(..) {
            // Work still here means this job was never answered: the leader unwound.
            if work.is_some() {
                if ticket.async_bytes > 0 {
                    self.async_failed.fetch_add(1, Ordering::Relaxed);
                }
                ticket.settle(Err(ApiError::Value(
                    "the writer that took this batch did not finish".to_string(),
                )));
            }
        }
    }
}

impl Group {
    /// Replaces the configuration. Takes effect from the next submission.
    pub(crate) fn configure(&self, config: GroupConfig) {
        *self.config.lock().unwrap_or_else(|p| p.into_inner()) = config;
        self.enabled.store(config.enabled, Ordering::Relaxed);
    }

    pub(crate) fn stats(&self) -> GroupStats {
        let (held, oldest) = {
            let queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            (queue.async_bytes as u64, queue.oldest_async)
        };
        GroupStats {
            commits: self.commits.load(Ordering::Relaxed),
            jobs: self.jobs.load(Ordering::Relaxed),
            isolations: self.isolations.load(Ordering::Relaxed),
            isolation_attempts: self.isolation_attempts.load(Ordering::Relaxed),
            async_held_bytes: held,
            async_oldest_seconds: oldest.map_or(0.0, |t| t.elapsed().as_secs_f64()),
            async_refused: self.async_refused.load(Ordering::Relaxed),
            async_failed: self.async_failed.load(Ordering::Relaxed),
        }
    }

    /// Whether answering before the commit is allowed at all.
    pub(crate) fn takes_async(&self) -> bool {
        self.enabled() && self.config.lock().unwrap_or_else(|p| p.into_inner()).async_writes
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Writes `work`, alone or in company, and answers when it is durable.
    ///
    /// The caller is parked between joining the queue and being told the answer, and there is
    /// no timeout on that park. Deliberate: a batch handed to a leader cannot be taken back, so
    /// a caller that gave up waiting would be reporting a failure for something about to land.
    pub(crate) fn submit<P: PagerMut + Sync>(
        &self,
        db: &Db<P>,
        work: Work,
        ack: Ack,
    ) -> Result<Accepted> {
        let config = *self.config.lock().unwrap_or_else(|p| p.into_inner());
        let work = {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            if queue.stopping {
                return Err(ApiError::Busy("this database is shutting down".to_string()));
            }

            // **Answering early is a queue push and nothing else.** The job goes in with a
            // ticket nobody parks on; whichever thread leads next takes it along, exactly as if
            // a caller were waiting behind it. That is why async needs no second code path -
            // only somebody to be the leader eventually, which is the flusher's whole job.
            if ack == Ack::Async && config.async_writes {
                let bytes = work.bytes();
                // An empty buffer takes whatever it is given, for the reason `max_facts` lets
                // an oversized batch go alone: a ceiling that refused a batch bigger than
                // itself would refuse it forever, and under `Refuse` that is a permanent error
                // for a request that is not wrong.
                let fits =
                    queue.async_bytes == 0 || queue.async_bytes + bytes <= config.max_async_bytes;
                if fits {
                    let count = work.weight() as u64;
                    queue.async_bytes += bytes;
                    queue.oldest_async.get_or_insert_with(Instant::now);
                    queue.pending.push_back(Ticket::unattended(work, bytes));
                    let idle = !queue.leading;
                    drop(queue);
                    // Only the first arrival has to wake anybody: a flusher already awake will
                    // take everything behind it in the same drain.
                    if idle {
                        self.arrived.notify_all();
                    }
                    return Ok(Accepted { count, txn: None });
                }
                if config.when_full == WhenFull::Refuse {
                    self.async_refused.fetch_add(1, Ordering::Relaxed);
                    return Err(ApiError::Busy(
                        "the write buffer is full; retry shortly".to_string(),
                    ));
                }
                // `WhenFull::Block`: fall through and be answered the durable way. The client
                // that filled the buffer is the one that waits, which is backpressure aimed at
                // the right place.
            }

            if queue.leading {
                let ticket = Ticket::new(work);
                queue.pending.push_back(Arc::clone(&ticket));
                drop(queue);
                match park(&ticket) {
                    Parked::Done(result) => return result,
                    Parked::Lead(work) => work,
                }
            } else {
                queue.leading = true;
                work
            }
        };
        let outcome = self.lead(db, work);
        // Elect a successor rather than looping: a leader that kept going could be pinned here
        // by a stream of newcomers, and one commit per thread is what keeps latency bounded.
        self.hand_over();
        outcome
    }

    /// One commit for this thread's work and whatever queued while it waited for the lock.
    fn lead<P: PagerMut + Sync>(&self, db: &Db<P>, own: Work) -> Result<Accepted> {
        let mine = Ticket::new(own);
        self.commit_once(db, Some(&mine));
        answer_of(&mine)
    }

    /// One commit for whatever is queued, with `mine` first if there is a `mine`.
    ///
    /// `None` is the flusher's case: nobody's own batch, just whatever has been acknowledged and
    /// is waiting to become durable. Returns how many jobs the commit carried.
    fn commit_once<P: PagerMut + Sync>(&self, db: &Db<P>, mine: Option<&Arc<Ticket>>) -> u64 {
        // **The accumulation window.** `Db::write` blocks on the store's write lock, and
        // everything that arrives while it does is company this commit gets for nothing.
        // Draining before this call would collect an empty queue; draining after a timer would
        // charge an idle server latency it does not have to pay.
        let mut w = db.write();
        let mut taken = self.take_group(mine);
        let len = taken.jobs.len();
        if len == 0 {
            return 0;
        }

        let counts = match apply_range(&mut w, &taken, 0, len) {
            Some(counts) => counts,
            None => {
                // Nothing reached the disk: `WriteTxn`'s destructor is the rollback.
                drop(w);
                self.split(db, &mut taken);
                return len as u64;
            }
        };
        match w.commit() {
            Ok(txn) => {
                self.commits.fetch_add(1, Ordering::Relaxed);
                self.jobs.fetch_add(len as u64, Ordering::Relaxed);
                for (i, count) in counts.into_iter().enumerate() {
                    taken.jobs[i].1 = None;
                    taken.settle(i, Ok(Accepted { count, txn: Some(txn) }));
                }
            }
            Err(_) => {
                // The commit failed, so nothing landed here either. Same treatment: find out
                // which job the storage layer objects to rather than blaming all of them.
                self.split(db, &mut taken);
            }
        }
        len as u64
    }

    /// Takes up to the caps off the queue, with this thread's own job first.
    ///
    /// First because it arrived first, and arrival order is what makes last-write-wins per
    /// record mean here what it means inside one transaction.
    fn take_group(&self, mine: Option<&Arc<Ticket>>) -> Taken<'_> {
        let config = *self.config.lock().unwrap_or_else(|p| p.into_inner());
        let mut jobs: Vec<(Arc<Ticket>, Option<Work>)> = Vec::new();
        let mut facts = 0usize;

        // Own work first, and the guard is dropped before the queue is locked: every other
        // path here takes the queue before a slot, and one place doing it the other way round
        // is how a deadlock gets written.
        if let Some(mine) = mine {
            let mut slot = mine.slot.lock().unwrap_or_else(|p| p.into_inner());
            if let Slot::Waiting(work) | Slot::Lead(work) =
                std::mem::replace(&mut *slot, Slot::Taken)
            {
                facts = work.weight();
                jobs.push((Arc::clone(mine), Some(work)));
            }
        }

        let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        while jobs.len() < config.max_jobs {
            let Some(ticket) = queue.pending.front().cloned() else { break };
            let mut slot = ticket.slot.lock().unwrap_or_else(|p| p.into_inner());
            let work = match std::mem::replace(&mut *slot, Slot::Taken) {
                Slot::Waiting(work) => work,
                // Not reachable today - a ticket leaves the queue before it is elected or
                // answered - but the value is put back rather than dropped, because dropping a
                // `Done` here would lose somebody's answer and park them for good.
                other => {
                    *slot = other;
                    drop(slot);
                    queue.pending.pop_front();
                    continue;
                }
            };
            let weight = work.weight();
            if !jobs.is_empty() && facts + weight > config.max_facts {
                *slot = Slot::Waiting(work);
                break;
            }
            facts += weight;
            drop(slot);
            queue.pending.pop_front();
            // Acknowledged work stops counting against the ceiling the moment it is in a
            // transaction, not when that transaction commits: it is no longer waiting, and a
            // ceiling that stayed occupied until the fsync would throttle the very commit that
            // empties it.
            queue.async_bytes = queue.async_bytes.saturating_sub(ticket.async_bytes);
            jobs.push((ticket, Some(work)));
        }
        // The oldest thing still waiting, recomputed rather than tracked: the queue is bounded
        // by how many callers there are, and a stale timestamp here is a lag graph that lies.
        queue.oldest_async = queue.pending.iter().find(|t| t.async_bytes > 0).map(|t| t.at);
        Taken { jobs, async_failed: &self.async_failed }
    }

    /// Elects the oldest waiter, or clears the flag if there is nobody.
    fn hand_over(&self) {
        let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let mut i = 0;
        while i < queue.pending.len() {
            // **Only somebody who is actually waiting can be made leader.** An acknowledged
            // write has no thread of its own, so electing it would set `leading` with nobody to
            // lead: every later writer would then queue behind a leader that does not exist,
            // and the flusher would decline to act because it saw one. Skipped, not removed -
            // it stays where it is in arrival order for whoever leads next.
            if queue.pending[i].async_bytes > 0 {
                i += 1;
                continue;
            }
            let ticket = queue.pending.remove(i).expect("the index was just in range");
            let mut slot = ticket.slot.lock().unwrap_or_else(|p| p.into_inner());
            match std::mem::replace(&mut *slot, Slot::Taken) {
                Slot::Waiting(work) => {
                    *slot = Slot::Lead(work);
                    ticket.wake.notify_one();
                    // `leading` stays true: it has been handed on, not released.
                    return;
                }
                // As in `take_group`: put it back rather than drop it.
                other => *slot = other,
            }
        }
        // Nobody is waiting. Anything still queued is acknowledged work, which the next writer
        // or the flusher will carry - so the flag has to come off, or neither of them would.
        queue.leading = false;
    }

    /// Commits whatever has been acknowledged and is still waiting.
    ///
    /// **Does nothing when somebody else is already leading**, and that is the right answer
    /// rather than a shortcut: a leader drains the whole queue, so work waiting now is work
    /// that commit is already going to carry. Fighting for the lock would buy a second
    /// transaction and no extra durability.
    ///
    /// Returns the jobs it committed.
    pub(crate) fn flush<P: PagerMut + Sync>(&self, db: &Db<P>) -> u64 {
        {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            if queue.leading || queue.pending.is_empty() {
                return 0;
            }
            queue.leading = true;
        }
        let carried = self.commit_once(db, None);
        self.hand_over();
        carried
    }

    /// Waits for acknowledged work to arrive, or for `linger` to pass.
    ///
    /// The wait is what keeps a flusher on an idle node from waking at all: nothing is queued,
    /// so nothing notifies, so it sleeps until the timeout and finds the queue still empty. The
    /// timeout exists so a stop is noticed and so the caller's own clock keeps ticking.
    pub(crate) fn await_work(&self, timeout: Duration) -> bool {
        let queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        if !queue.pending.is_empty() || queue.stopping {
            return true;
        }
        let (queue, _) =
            self.arrived.wait_timeout(queue, timeout).unwrap_or_else(|p| p.into_inner());
        !queue.pending.is_empty() || queue.stopping
    }

    /// How long an acknowledged write may wait before a flusher commits it.
    pub(crate) fn linger(&self) -> Duration {
        self.config.lock().unwrap_or_else(|p| p.into_inner()).linger
    }

    /// Refuses new work and commits what is held.
    ///
    /// **The order is the point.** New submissions are refused first, so the drain below runs
    /// against a queue that cannot grow; then everything left is committed, isolation included,
    /// because a batch this database refuses at shutdown must not cost the sixty-three beside
    /// it. Idempotent, so a caller that stops twice is not a caller with a bug.
    pub(crate) fn stop<P: PagerMut + Sync>(&self, db: &Db<P>) -> u64 {
        self.queue.lock().unwrap_or_else(|p| p.into_inner()).stopping = true;
        self.arrived.notify_all();

        let mut carried = 0;
        // A loop rather than one pass: `max_jobs` bounds a commit, so a queue deeper than the
        // cap needs more than one of them, and shutdown is exactly when leaving some behind
        // would be losing data somebody was told was accepted.
        loop {
            let flushed = self.flush(db);
            if flushed > 0 {
                carried += flushed;
                continue;
            }
            // Nothing carried. Either there is nothing left, or somebody else is mid-commit -
            // and a commit always finishes, so waiting for them terminates.
            let queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            if queue.pending.is_empty() {
                break;
            }
            drop(queue);
            std::thread::yield_now();
        }
        carried
    }

    /// Called when the whole group has just failed, so it halves immediately.
    ///
    /// Going through [`Group::isolate_range`] for the full range would re-attempt exactly what
    /// was tried a moment ago and is known not to work - one wasted transaction per failure,
    /// paid every time.
    fn split<P: PagerMut + Sync>(&self, db: &Db<P>, taken: &mut Taken<'_>) {
        self.isolations.fetch_add(1, Ordering::Relaxed);
        let len = taken.jobs.len();
        if len <= 1 {
            self.isolate_range(db, taken, 0, len);
            return;
        }
        let mid = len / 2;
        self.isolate_range(db, taken, 0, mid);
        self.isolate_range(db, taken, mid, len);
    }

    /// Splits a failed group into contiguous halves until the offender is alone.
    ///
    /// **Contiguous, and the left half first.** A group is applied in arrival order, so a split
    /// that reordered jobs would change what last-write-wins on one record means — a wrong
    /// answer rather than a slow one.
    ///
    /// Cheaper than one-at-a-time for the reason that actually matters: an attempt that *fails*
    /// drops its transaction before `write_and_flip`, so it costs no fsync at all. Splitting
    /// spends at most `2n-1` attempts in order to spend at most `n` real commits, and on the
    /// realistic shape — one bad batch among many good ones — far fewer of both than isolating
    /// one at a time, which always pays `n` commits.
    fn isolate_range<P: PagerMut + Sync>(
        &self,
        db: &Db<P>,
        taken: &mut Taken<'_>,
        from: usize,
        to: usize,
    ) {
        if from >= to {
            return;
        }
        self.isolation_attempts.fetch_add(1, Ordering::Relaxed);
        let mut w = db.write();
        if let Some(counts) = apply_range(&mut w, taken, from, to) {
            if let Ok(txn) = w.commit() {
                self.commits.fetch_add(1, Ordering::Relaxed);
                self.jobs.fetch_add((to - from) as u64, Ordering::Relaxed);
                for (i, count) in (from..to).zip(counts) {
                    taken.jobs[i].1 = None;
                    taken.settle(i, Ok(Accepted { count, txn: Some(txn) }));
                }
                return;
            }
        } else {
            drop(w);
        }

        if to - from == 1 {
            // Alone and still failing. Run it once more by itself so the caller is handed the
            // real error rather than a synthesised one: `ApiError` is not `Clone` —
            // `StoreError::Io` wraps `std::io::Error` — so re-running is what produces a
            // genuine error per waiter without putting an `Arc` in a public signature.
            let Some(job) = taken.jobs[from].1.take() else { return };
            let mut w = db.write();
            let result = apply_one(&mut w, &job).and_then(|count| {
                let txn = w.commit()?;
                self.commits.fetch_add(1, Ordering::Relaxed);
                self.jobs.fetch_add(1, Ordering::Relaxed);
                Ok(Accepted { count, txn: Some(txn) })
            });
            taken.settle(from, result);
            return;
        }

        let mid = from + (to - from) / 2;
        self.isolate_range(db, taken, from, mid);
        self.isolate_range(db, taken, mid, to);
    }
}

enum Parked {
    Done(Result<Accepted>),
    Lead(Work),
}

/// Waits until this ticket is answered or elected.
fn park(ticket: &Arc<Ticket>) -> Parked {
    let mut slot = ticket.slot.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        match std::mem::replace(&mut *slot, Slot::Taken) {
            Slot::Done(result) => return Parked::Done(result),
            Slot::Lead(work) => return Parked::Lead(work),
            other => {
                // Put it back and keep waiting. `replace` is the only way to move the work out,
                // and the two arms above are the only ones that end the wait.
                *slot = other;
                slot = ticket.wake.wait(slot).unwrap_or_else(|p| p.into_inner());
            }
        }
    }
}

/// Reads back the answer a leader wrote to its own ticket.
fn answer_of(mine: &Arc<Ticket>) -> Result<Accepted> {
    mine.answer().unwrap_or_else(|| {
        Err(ApiError::Value("the writer did not answer its own batch".to_string()))
    })
}

/// Applies `[from, to)` into an open transaction, or says which job refused.
///
/// `None` rather than the index, because every caller does the same thing with it: drop the
/// transaction and split. A caller that wanted the index would still have to re-run to get a
/// reportable error out of it.
fn apply_range<P: PagerMut>(
    w: &mut DbWrite<'_, P>,
    taken: &Taken<'_>,
    from: usize,
    to: usize,
) -> Option<Vec<u64>> {
    let mut counts = Vec::with_capacity(to - from);
    for i in from..to {
        match taken.jobs[i].1.as_ref() {
            // Already answered by an earlier split. It contributes nothing and must not be
            // written twice.
            None => counts.push(0),
            Some(work) => match apply_one(w, work) {
                Ok(count) => counts.push(count),
                Err(_) => return None,
            },
        }
    }
    Some(counts)
}

/// One job into an open transaction, exactly as the uncoalesced call would have written it.
fn apply_one<P: PagerMut>(w: &mut DbWrite<'_, P>, work: &Work) -> Result<u64> {
    match work {
        Work::Import { table, batch } => {
            let facts = batch.facts();
            crate::apply(w, table, &facts)?;
            Ok(facts.len() as u64)
        }
        Work::ImportWithKeys { table, keys, batch } => {
            for k in keys {
                w.assign_key(table.as_str(), &k.field, &k.key, k.row)?;
            }
            let facts = batch.facts();
            crate::apply(w, table, &facts)?;
            Ok(facts.len() as u64)
        }
        Work::Delete { table, records } => Ok(w.delete(table.as_str(), records)?),
    }
}
