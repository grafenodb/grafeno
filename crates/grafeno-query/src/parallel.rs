//! Parallel scans: the candidates of a large start split across threads.
//!
//! A read-only `MATCH` that starts from many nodes (a large label, or an
//! index range that holds many of them) hands chunks of the candidates to
//! worker threads, which share the snapshot and the query's budget (its
//! limits count every thread's reads, and time out or cancel them all).
//! Each chunk's rows are handed on in chunk order, as soon as the chunks
//! before it are done, so the rows are the same, in the same order, as on
//! one thread, and a batched aggregation folds them while the scan runs.
//!
//! Threads are shared by every query of the process: at most one per CPU
//! (or [`set_parallelism`]) work at once, so many concurrent queries do
//! not multiply them; a query that finds none free runs on its own thread.
//! Work inside a worker (a pattern predicate, a subquery) does not split
//! again.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::QueryError;
use crate::eval::Context;

/// Starts with fewer candidates than this run on one thread.
pub const MIN_CANDIDATES: usize = 4096;
/// Candidates per chunk handed to a worker, at least.
const MIN_CHUNK: usize = 512;

/// The most threads parallel scans may use at once; 0 is one per CPU.
static PARALLELISM: AtomicUsize = AtomicUsize::new(0);
/// Worker threads running now, over every query.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static IN_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Sets how many threads parallel scans may use at once, over all
/// queries (1 turns them off; 0 is one per CPU, the default).
pub fn set_parallelism(threads: usize) {
    PARALLELISM.store(threads, Ordering::Relaxed);
}

/// How many threads parallel scans may use at once.
pub fn parallelism() -> usize {
    match PARALLELISM.load(Ordering::Relaxed) {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    }
}

/// Whether this thread may split a scan now.
pub(crate) fn available() -> bool {
    parallelism() > 1 && !IN_WORKER.with(Cell::get)
}

/// Takes up to `want` worker threads from the shared allowance.
fn reserve(want: usize) -> usize {
    let max = parallelism().saturating_sub(1);
    let mut current = ACTIVE.load(Ordering::Relaxed);
    loop {
        let take = want.min(max.saturating_sub(current));
        if take == 0 {
            return 0;
        }
        match ACTIVE.compare_exchange(current, current + take, Ordering::AcqRel, Ordering::Relaxed)
        {
            Ok(_) => return take,
            Err(now) => current = now,
        }
    }
}

struct Reserved(usize);

impl Drop for Reserved {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(self.0, Ordering::AcqRel);
    }
}

/// Marks the current thread as a worker until dropped.
struct WorkerMark(bool);

impl WorkerMark {
    fn new() -> Self {
        Self(IN_WORKER.with(|flag| flag.replace(true)))
    }
}

impl Drop for WorkerMark {
    fn drop(&mut self) {
        IN_WORKER.with(|flag| flag.set(self.0));
    }
}

/// Chunks done but not yet handed over, waiting for the ones before them.
struct Delivery<'t, T> {
    next: usize,
    ready: BTreeMap<usize, T>,
    take: &'t mut (dyn FnMut(T) -> Result<(), QueryError> + Send),
    /// The first error, by chunk position.
    error: Option<(usize, QueryError)>,
}

impl<T> Delivery<'_, T> {
    fn fail(&mut self, at: usize, error: QueryError) {
        if self.error.as_ref().is_none_or(|(first, _)| at < *first) {
            self.error = Some((at, error));
        }
    }
}

/// Runs `work` on chunks of `items` (candidate ids, or the partial
/// matches of a large frontier) on this thread and the workers it can
/// get, and hands each chunk's result to `take` in chunk order, as soon as
/// the chunks before it are done (so results do not pile up). The first
/// error, in chunk order, stops the rest.
pub(crate) fn map_chunks<I, T, F>(
    ctx: &Context,
    ids: &[I],
    work: F,
    take: &mut (dyn FnMut(T) -> Result<(), QueryError> + Send),
) -> Result<(), QueryError>
where
    I: Sync,
    T: Send,
    F: Fn(&Context, &[I]) -> Result<T, QueryError> + Sync,
{
    let wanted = (ids.len() / MIN_CHUNK).saturating_sub(1);
    let reserved = Reserved(if ids.len() >= MIN_CANDIDATES {
        reserve(wanted)
    } else {
        0
    });
    let _mark = WorkerMark::new();
    if reserved.0 == 0 {
        for chunk in ids.chunks(MIN_CHUNK) {
            take(work(ctx, chunk)?)?;
        }
        return Ok(());
    }
    let threads = reserved.0 + 1;
    // Several chunks per thread, so a slow chunk does not hold the rest.
    let size = ids.len().div_ceil(threads * 8).max(MIN_CHUNK);
    let chunks: Vec<&[I]> = ids.chunks(size).collect();
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let delivery = Mutex::new(Delivery {
        next: 0,
        ready: BTreeMap::new(),
        take,
        error: None,
    });
    let run = |ctx: &Context| {
        loop {
            if failed.load(Ordering::Relaxed) {
                break;
            }
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(chunk) = chunks.get(index) else {
                break;
            };
            let result = work(ctx, chunk);
            let mut delivery = delivery.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(value) => {
                    delivery.ready.insert(index, value);
                }
                Err(error) => {
                    delivery.fail(index, error);
                    failed.store(true, Ordering::Relaxed);
                    continue;
                }
            }
            // Hand over every chunk that is next in line.
            while delivery.error.is_none() {
                let at = delivery.next;
                let Some(value) = delivery.ready.remove(&at) else {
                    break;
                };
                delivery.next += 1;
                if let Err(error) = (delivery.take)(value) {
                    delivery.fail(at, error);
                    failed.store(true, Ordering::Relaxed);
                }
            }
        }
    };
    let (read, ns, params, edges, info, budget, view, may_audit) = (
        ctx.read,
        ctx.ns,
        ctx.params,
        ctx.edges,
        ctx.info,
        ctx.budget,
        ctx.view,
        ctx.may_audit,
    );
    std::thread::scope(|scope| {
        for _ in 1..threads {
            scope.spawn(|| {
                let _mark = WorkerMark::new();
                let ctx = Context {
                    read,
                    write: None,
                    ns,
                    params,
                    edges,
                    info,
                    budget,
                    view,
                    may_audit,
                    // Traced statements run on one thread.
                    trace: None,
                };
                run(&ctx);
            });
        }
        run(ctx);
    });
    let delivery = delivery.into_inner().unwrap_or_else(|e| e.into_inner());
    match delivery.error {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}
