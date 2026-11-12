//! A persistent worker pool for data-parallel passes over borrowed data. Local displaylink-lite
//! patch; see vendor/dlcore/ORIGIN.md.
//!
//! Full-screen motion runs several such passes per frame (capture conversion, strip hashing,
//! strip encoding); spawning fresh scoped threads for each one cost about as much CPU as a small
//! pass itself. Here threads are spawned once and park on a condition variable between passes.
//!
//! Callers never wait for the pool to become free: every caller works through its own pass on
//! its own thread, and idle workers join in. So concurrent and nested callers cannot deadlock,
//! and a busy pool only means a caller does more of its pass itself.

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};

/// Threads worth giving a pass that streams memory with little arithmetic per byte (hashing,
/// pixel conversion). Beyond this, extra threads (mostly efficiency cores) added CPU time without
/// cutting wall time: a 4K capture conversion on an 18-core M5 Pro took 4.2 ms CPU on 18 threads
/// and 3.5 ms on 8, and RGB strip hashing 1.3 ms and 0.8 ms.
pub const STREAMING_THREADS: usize = 8;

/// One pass: indices `0..count`, each claimed exactly once by the caller or a worker.
struct Job<'a> {
    f: &'a (dyn Fn(usize) + Sync),
    count: usize,
    next: AtomicUsize,
    /// Most workers that may join besides the caller.
    helpers: usize,
    /// Workers currently inside this job. Only changed with the pool lock held, so a caller
    /// that sees zero under the lock knows no worker will touch the job again.
    joined: AtomicUsize,
    panicked: AtomicBool,
}

impl Job<'_> {
    fn wants_help(&self) -> bool {
        self.next.load(Ordering::Relaxed) < self.count && self.joined.load(Ordering::Relaxed) < self.helpers
    }

    fn run(&self) {
        loop {
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            if i >= self.count {
                return;
            }
            (self.f)(i);
        }
    }
}

/// A job's address with its borrow erased. Sound because [`run`] removes the job from the pool
/// and waits for every worker inside it to leave before the borrow ends.
#[derive(Clone, Copy, PartialEq)]
struct JobPtr(*const Job<'static>);

// SAFETY: a `Job` is only shared through `&`, and all of its fields are `Sync`.
unsafe impl Send for JobPtr {}

struct Pool {
    /// Jobs whose callers are still claiming indices.
    open: Mutex<Vec<JobPtr>>,
    /// Signalled when a job is opened.
    work: Condvar,
    /// Signalled when the last worker leaves a job.
    left: Condvar,
    workers: usize,
}

impl Pool {
    fn lock(&self) -> MutexGuard<'_, Vec<JobPtr>> {
        // Nothing panics with the lock held, but a poisoned list is still a valid one.
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The process-wide pool: one worker per core besides the caller's, spawned on first use.
static POOL: LazyLock<Pool> = LazyLock::new(|| {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    // Workers block on `POOL` until this initializer returns. Callers do their own work, so a
    // pool short of threads is merely slower.
    let workers = (1..cores)
        .take_while(|n| std::thread::Builder::new().name(format!("vino-pool-{n}")).spawn(|| worker(&POOL)).is_ok())
        .count();
    Pool { open: Mutex::new(Vec::new()), work: Condvar::new(), left: Condvar::new(), workers }
});

fn worker(pool: &Pool) {
    let mut open = pool.lock();
    loop {
        // SAFETY: a job in `open` is alive; its caller cannot remove it without this lock.
        let Some(ptr) = open.iter().copied().find(|j| unsafe { &*j.0 }.wants_help()) else {
            open = pool.work.wait(open).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        // SAFETY: as above; once `joined` counts this worker, the caller waits for it to leave.
        let job = unsafe { &*ptr.0 };
        job.joined.fetch_add(1, Ordering::Relaxed);
        drop(open);
        if panic::catch_unwind(AssertUnwindSafe(|| job.run())).is_err() {
            job.panicked.store(true, Ordering::Relaxed);
        }
        open = pool.lock();
        if job.joined.fetch_sub(1, Ordering::Relaxed) == 1 {
            pool.left.notify_all();
        }
    }
}

/// Number of threads a pass can run on: the pool's workers plus the caller.
pub fn threads() -> usize {
    POOL.workers + 1
}

/// Call `f(i)` for every `i` in `0..count` on at most `threads` threads (the caller's and pool
/// workers'), returning once every call has finished. A panic in any call is resumed here once
/// all have finished.
pub fn run(count: usize, threads: usize, f: &(dyn Fn(usize) + Sync)) {
    let pool = &*POOL;
    let helpers = count.min(threads).saturating_sub(1).min(pool.workers);
    if helpers == 0 {
        (0..count).for_each(f);
        return;
    }
    let job = Job {
        f,
        count,
        next: AtomicUsize::new(0),
        helpers,
        joined: AtomicUsize::new(0),
        panicked: AtomicBool::new(false),
    };
    let ptr = JobPtr((&job as *const Job<'_>).cast());

    /// Closes the job and waits out its workers, even if the caller's own share panics.
    struct Close<'a> {
        pool: &'a Pool,
        job: &'a Job<'a>,
        ptr: JobPtr,
    }
    impl Drop for Close<'_> {
        fn drop(&mut self) {
            let mut open = self.pool.lock();
            open.retain(|&j| j != self.ptr);
            while self.job.joined.load(Ordering::Relaxed) > 0 {
                open = self.pool.left.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    pool.lock().push(ptr);
    let close = Close { pool, job: &job, ptr };
    if helpers == pool.workers {
        pool.work.notify_all();
    } else {
        (0..helpers).for_each(|_| pool.work.notify_one());
    }
    job.run();
    drop(close);
    if job.panicked.load(Ordering::Relaxed) {
        panic!("a pool worker panicked");
    }
}

/// A slice's base pointer, shareable because [`for_each_mut`] hands each item to one call only.
struct Items<T>(*mut T);

// SAFETY: every item is reached by exactly one call, so sharing the base only moves `T`s
// between threads, which `T: Send` allows.
unsafe impl<T: Send> Sync for Items<T> {}

impl<T> Items<T> {
    // A method, so closures capture the whole `Items` rather than its bare pointer field.
    fn ptr(&self) -> *mut T {
        self.0
    }
}

/// Call `f(index, item)` for every item on at most `threads` threads. Items are claimed in
/// contiguous runs, about four per thread, so uneven items still balance.
pub fn for_each_mut<T: Send>(items: &mut [T], threads: usize, f: impl Fn(usize, &mut T) + Sync) {
    let len = items.len();
    let run = len.div_ceil(threads.max(1) * 4).max(1);
    let base = Items(items.as_mut_ptr());
    self::run(len.div_ceil(run), threads, &|claim| {
        let start = claim * run;
        let end = (start + run).min(len);
        // SAFETY: each claim is made once and claims cover disjoint ranges within `len`, and
        // `items` stays mutably borrowed until every call has returned.
        let items = unsafe { std::slice::from_raw_parts_mut(base.ptr().add(start), end - start) };
        for (offset, item) in items.iter_mut().enumerate() {
            f(start + offset, item);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_once() {
        for len in [0usize, 1, 2, 7, 64, 1000] {
            for threads in [1usize, 2, 3, 64] {
                let mut items = vec![0usize; len];
                for_each_mut(&mut items, threads, |i, item| *item += i + 1);
                assert!(items.iter().enumerate().all(|(i, &v)| v == i + 1), "len {len} threads {threads}");
            }
        }
    }

    #[test]
    fn concurrent_and_nested_callers_finish() {
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        let mut outer = vec![0u64; 32];
                        for_each_mut(&mut outer, threads(), |i, sum| {
                            let mut inner = vec![0u64; 16];
                            for_each_mut(&mut inner, threads(), |j, v| *v = (i * j) as u64);
                            *sum = inner.iter().sum();
                        });
                        let want: Vec<u64> = (0..32u64).map(|i| i * (0..16).sum::<u64>()).collect();
                        assert_eq!(outer, want);
                    }
                });
            }
        });
    }

    #[test]
    fn panic_reaches_caller_after_all_calls() {
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            run(256, threads(), &|i| {
                if i == 100 {
                    panic!("boom");
                }
            })
        }));
        assert!(result.is_err());
        // The pool still serves passes afterwards.
        let mut items = vec![0u8; 300];
        for_each_mut(&mut items, threads(), |_, v| *v = 1);
        assert!(items.iter().all(|&v| v == 1));
    }
}
