//! Group commit: concurrent writes share one transaction and one commit.
//!
//! redb has one writer at a time, and a commit with immediate durability
//! waits for the disk. A [`Writer`] runs on its own thread: it takes every
//! write waiting in its queue (up to a batch size), runs them one after the
//! other in a single transaction, and commits once. Each write still gets
//! its own result:
//!
//! - writes run in arrival order, and each sees the writes before it;
//! - when one fails, the transaction is discarded, that write gets its
//!   error, and the others run again in a new transaction (so a write must
//!   be safe to run twice: it is given as `Fn`, and only its last run
//!   counts);
//! - when the commit fails, every write of the group gets that error.
//!
//! Writes come in two lanes. Interactive writes (an agent remembering
//! something) always go first. Bulk writes (an import, a rebuild) run one
//! per transaction, between groups of interactive writes, so a long load
//! delays an interactive write by at most one bulk transaction instead of
//! holding the database until it ends.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use grafeno_core::{Error, Result};
use serde::Serialize;

use crate::db::{Grafeno, WriteTx};

/// How a [`Writer`] groups writes.
#[derive(Clone, Copy, Debug)]
pub struct WriterOptions {
    /// Interactive writes per transaction at most. Under heavy load the
    /// groups reach it, so it bounds throughput: 512 commits about 11 000
    /// writes a second on a laptop SSD with immediate durability, 128 about
    /// 8 000.
    pub max_batch: usize,
    /// Interactive writes that may wait; beyond it, [`Writer::write`]
    /// answers [`Error::Busy`] at once.
    pub queue: usize,
    /// Bulk writes that may wait; beyond it, [`Writer::write_bulk`] waits
    /// for room (a bulk load slows down instead of failing).
    pub bulk_queue: usize,
    /// With eventual durability, how often waiting commits reach the disk.
    pub flush_every: Duration,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            max_batch: 512,
            queue: 4_096,
            bulk_queue: 4,
            flush_every: Duration::from_millis(500),
        }
    }
}

/// Which queue a write waits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Short writes someone waits for: grouped, and served first.
    Interactive,
    /// Long writes (loads): one per transaction, after the interactive ones,
    /// committed with eventual durability (see [`Writer::flush`]).
    Bulk,
}

/// What a write did, kept while the group is not committed.
type Attempt = Box<dyn FnMut(&mut WriteTx) -> Result<()> + Send>;
type Done = Box<dyn FnOnce(Result<()>) + Send>;

struct Job {
    attempt: Attempt,
    done: Done,
    queued: Instant,
}

#[derive(Default)]
struct Queues {
    interactive: VecDeque<Job>,
    bulk: VecDeque<Job>,
    closed: bool,
}

struct Shared {
    queues: Mutex<Queues>,
    /// Signalled when a job arrives or the writer closes.
    ready: Condvar,
    /// Signalled when a bulk job leaves its queue.
    room: Condvar,
    metrics: WriterMetrics,
}

/// A thread that commits writes in groups. Dropping it lets the queued
/// writes finish and stops the thread.
pub struct Writer {
    shared: Arc<Shared>,
    options: WriterOptions,
    worker: Option<JoinHandle<()>>,
    db: Arc<Grafeno>,
}

impl Writer {
    pub fn new(db: Arc<Grafeno>, options: WriterOptions) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::default(),
            ready: Condvar::new(),
            room: Condvar::new(),
            metrics: WriterMetrics::default(),
        });
        let worker = {
            let (db, shared) = (db.clone(), shared.clone());
            std::thread::Builder::new()
                .name("grafeno-writer".into())
                .spawn(move || work(&db, &shared, options))
                .expect("the writer thread starts")
        };
        Self {
            shared,
            options,
            worker: Some(worker),
            db,
        }
    }

    /// What the writer has done so far, and what waits.
    pub fn metrics(&self) -> WriterSnapshot {
        let queues = self.shared.queues.lock().unwrap_or_else(|e| e.into_inner());
        self.shared
            .metrics
            .snapshot(queues.interactive.len(), queues.bulk.len())
    }

    /// Runs `f` in the next interactive group and waits for its commit. `f`
    /// may run more than once (when another write of its group fails);
    /// only the last run's changes and value count.
    pub fn write<T: Send + 'static>(
        &self,
        f: impl Fn(&mut WriteTx) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.submit(Lane::Interactive, f)
    }

    /// Runs `f` in a transaction of its own, after the interactive writes
    /// waiting, and waits for its commit. The commit has eventual
    /// durability: call [`Writer::flush`] after the last bulk write.
    pub fn write_bulk<T: Send + 'static>(
        &self,
        f: impl Fn(&mut WriteTx) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.submit(Lane::Bulk, f)
    }

    /// Queues `f` for the next interactive group and returns at once:
    /// `then` gets its result on the writer's thread once its group is
    /// committed (or failed). Nobody waits on a thread meanwhile, so many
    /// writes may wait without holding as many threads. Fails at once,
    /// without calling `then`, when too many writes are waiting.
    pub fn write_then<T: Send + 'static>(
        &self,
        f: impl Fn(&mut WriteTx) -> Result<T> + Send + 'static,
        then: impl FnOnce(Result<T>) + Send + 'static,
    ) -> Result<()> {
        let job = job(f, then);
        {
            let mut queues = self.shared.queues.lock().unwrap_or_else(|e| e.into_inner());
            if queues.closed {
                return Err(Error::Storage("the writer stopped".into()));
            }
            if queues.interactive.len() >= self.options.queue.max(1) {
                self.shared.metrics.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Busy("too many writes are waiting".into()));
            }
            queues.interactive.push_back(job);
        }
        self.shared.ready.notify_one();
        Ok(())
    }

    /// The database it writes.
    pub fn database(&self) -> &Grafeno {
        &self.db
    }

    /// Makes every commit so far durable.
    pub fn flush(&self) -> Result<()> {
        self.db.flush()
    }

    fn submit<T: Send + 'static>(
        &self,
        lane: Lane,
        f: impl Fn(&mut WriteTx) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let job = job(f, move |result| {
            let _ = sender.send(result);
        });
        {
            let mut queues = self.shared.queues.lock().unwrap_or_else(|e| e.into_inner());
            if queues.closed {
                return Err(Error::Storage("the writer stopped".into()));
            }
            match lane {
                Lane::Interactive => {
                    if queues.interactive.len() >= self.options.queue.max(1) {
                        self.shared.metrics.rejected.fetch_add(1, Ordering::Relaxed);
                        return Err(Error::Busy("too many writes are waiting".into()));
                    }
                    queues.interactive.push_back(job);
                }
                Lane::Bulk => {
                    while queues.bulk.len() >= self.options.bulk_queue.max(1) && !queues.closed {
                        queues = self
                            .shared
                            .room
                            .wait(queues)
                            .unwrap_or_else(|e| e.into_inner());
                    }
                    queues.bulk.push_back(job);
                }
            }
        }
        self.shared.ready.notify_one();
        receiver
            .recv()
            .unwrap_or_else(|_| Err(Error::Storage("the writer stopped".into())))
    }
}

/// A queued write: `f`, whose last successful run's value goes to `then`.
fn job<T: Send + 'static>(
    f: impl Fn(&mut WriteTx) -> Result<T> + Send + 'static,
    then: impl FnOnce(Result<T>) + Send + 'static,
) -> Job {
    let slot = Arc::new(Mutex::new(None));
    let kept = slot.clone();
    Job {
        attempt: Box::new(move |tx| {
            let value = f(tx)?;
            *kept.lock().expect("not poisoned") = Some(value);
            Ok(())
        }),
        done: Box::new(move |outcome| {
            let result = outcome.map(|()| {
                slot.lock()
                    .expect("not poisoned")
                    .take()
                    .expect("a committed write has a value")
            });
            then(result);
        }),
        queued: Instant::now(),
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the queues ends the thread once it has drained them.
        self.shared
            .queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed = true;
        self.shared.ready.notify_all();
        self.shared.room.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The next work: a group of interactive writes, or one bulk write.
enum Next {
    Interactive(Vec<Job>),
    Bulk(Job),
    Idle,
    Closed,
}

fn next(shared: &Shared, options: &WriterOptions) -> Next {
    let mut queues = shared.queues.lock().unwrap_or_else(|e| e.into_inner());
    if queues.interactive.is_empty() && queues.bulk.is_empty() && !queues.closed {
        queues = shared
            .ready
            .wait_timeout(queues, options.flush_every)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    if !queues.interactive.is_empty() {
        let take = queues.interactive.len().min(options.max_batch.max(1));
        return Next::Interactive(queues.interactive.drain(..take).collect());
    }
    if let Some(job) = queues.bulk.pop_front() {
        shared.room.notify_one();
        return Next::Bulk(job);
    }
    if queues.closed {
        Next::Closed
    } else {
        Next::Idle
    }
}

fn work(db: &Grafeno, shared: &Shared, options: WriterOptions) {
    let mut last_flush = Instant::now();
    loop {
        match next(shared, &options) {
            Next::Interactive(batch) => run(db, &shared.metrics, batch, false),
            Next::Bulk(job) => run(db, &shared.metrics, vec![job], true),
            Next::Idle => {
                let _ = db.flush();
                last_flush = Instant::now();
                continue;
            }
            Next::Closed => break,
        }
        if last_flush.elapsed() >= options.flush_every {
            let _ = db.flush();
            last_flush = Instant::now();
        }
    }
    let _ = db.flush();
}

/// Runs a group to its commit, dropping the writes that fail.
fn run(db: &Grafeno, metrics: &WriterMetrics, mut jobs: Vec<Job>, bulk: bool) {
    let finish = |job: Job, outcome: Result<()>| {
        let waited = job.queued.elapsed();
        if bulk {
            metrics.bulk_latency.observe(waited.as_secs_f64());
        } else {
            metrics.latency.observe(waited.as_secs_f64());
        }
        if outcome.is_err() {
            metrics.failed.fetch_add(1, Ordering::Relaxed);
        }
        metrics.writes.fetch_add(1, Ordering::Relaxed);
        (job.done)(outcome);
    };
    let mut first = true;
    while !jobs.is_empty() {
        if !first {
            metrics
                .retried
                .fetch_add(jobs.len() as u64, Ordering::Relaxed);
        }
        first = false;
        let mut tx = match db.begin_write_lane(bulk) {
            Ok(tx) => tx,
            Err(error) => {
                for job in jobs {
                    finish(job, Err(error.clone()));
                }
                return;
            }
        };
        let mut failed = None;
        for (index, job) in jobs.iter_mut().enumerate() {
            match catch_unwind(AssertUnwindSafe(|| (job.attempt)(&mut tx))) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failed = Some((index, error));
                    break;
                }
                Err(_) => {
                    failed = Some((index, Error::Storage("the write panicked".into())));
                    break;
                }
            }
        }
        match failed {
            Some((index, error)) => {
                let _ = tx.abort();
                let job = jobs.remove(index);
                finish(job, Err(error));
            }
            None => {
                let started = Instant::now();
                let outcome = tx.commit();
                metrics.commit.observe(started.elapsed().as_secs_f64());
                metrics.group_size.observe(jobs.len() as f64);
                metrics.groups.fetch_add(1, Ordering::Relaxed);
                for job in jobs {
                    finish(job, outcome.clone());
                }
                return;
            }
        }
    }
}

// ---------------------------------------------------------------- metrics

/// Counts of observations in buckets, Prometheus style: `counts[i]` holds
/// those at most `bounds[i]` (and above the bound before it); the last
/// count is for everything above the last bound.
pub struct Histogram {
    bounds: &'static [f64],
    counts: Vec<AtomicU64>,
    /// The sum of observations, in millionths.
    sum: AtomicU64,
}

/// Seconds, from 50 µs to 10 s.
const SECONDS: &[f64] = &[
    0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5,
    1.0, 2.5, 5.0, 10.0,
];
/// Writes per group.
const SIZES: &[f64] = &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0];

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicU64::new(0),
        }
    }

    pub fn seconds() -> Self {
        Self::new(SECONDS)
    }

    pub fn observe(&self, value: f64) {
        let at = self
            .bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(self.bounds.len());
        self.counts[at].fetch_add(1, Ordering::Relaxed);
        self.sum
            .fetch_add((value * 1e6).max(0.0) as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            bounds: self.bounds.to_vec(),
            counts: self
                .counts
                .iter()
                .map(|count| count.load(Ordering::Relaxed))
                .collect(),
            sum: self.sum.load(Ordering::Relaxed) as f64 / 1e6,
        }
    }
}

/// A histogram at one moment.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HistogramSnapshot {
    pub bounds: Vec<f64>,
    /// Per bucket, not cumulative; one more than `bounds` (above the last).
    pub counts: Vec<u64>,
    pub sum: f64,
}

impl HistogramSnapshot {
    pub fn count(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The bound of the bucket holding the `q` quantile (0 to 1): an upper
    /// estimate. Infinity when it is above the last bound.
    pub fn quantile(&self, q: f64) -> f64 {
        let total = self.count();
        if total == 0 {
            return 0.0;
        }
        let target = (q * total as f64).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (at, count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= target {
                return self.bounds.get(at).copied().unwrap_or(f64::INFINITY);
            }
        }
        f64::INFINITY
    }
}

struct WriterMetrics {
    writes: AtomicU64,
    failed: AtomicU64,
    rejected: AtomicU64,
    retried: AtomicU64,
    groups: AtomicU64,
    group_size: Histogram,
    commit: Histogram,
    latency: Histogram,
    bulk_latency: Histogram,
}

impl Default for WriterMetrics {
    fn default() -> Self {
        Self {
            writes: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            retried: AtomicU64::new(0),
            groups: AtomicU64::new(0),
            group_size: Histogram::new(SIZES),
            commit: Histogram::seconds(),
            latency: Histogram::seconds(),
            bulk_latency: Histogram::seconds(),
        }
    }
}

impl WriterMetrics {
    fn snapshot(&self, queued: usize, queued_bulk: usize) -> WriterSnapshot {
        WriterSnapshot {
            queued,
            queued_bulk,
            writes: self.writes.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            retried: self.retried.load(Ordering::Relaxed),
            groups: self.groups.load(Ordering::Relaxed),
            group_size: self.group_size.snapshot(),
            commit_seconds: self.commit.snapshot(),
            latency_seconds: self.latency.snapshot(),
            bulk_latency_seconds: self.bulk_latency.snapshot(),
        }
    }
}

/// What a [`Writer`] did since it started, and what waits now.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WriterSnapshot {
    /// Interactive and bulk writes waiting.
    pub queued: usize,
    pub queued_bulk: usize,
    /// Writes done (committed or failed), writes that failed, writes
    /// refused because the queue was full, and runs again after another
    /// write of the group failed.
    pub writes: u64,
    pub failed: u64,
    pub rejected: u64,
    pub retried: u64,
    /// Transactions committed, and writes per transaction.
    pub groups: u64,
    pub group_size: HistogramSnapshot,
    pub commit_seconds: HistogramSnapshot,
    /// From a write's arrival to its result: interactive and bulk.
    pub latency_seconds: HistogramSnapshot,
    pub bulk_latency_seconds: HistogramSnapshot,
}
