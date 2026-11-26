//! The threads write-path validation runs on.
//!
//! Validation is CPU work — a structural walk, FHIRPath invariants, binding
//! checks answered mostly from memory — of about a millisecond per resource.
//! It used to run on the request's async task: a 450-entry transaction kept
//! one Tokio worker busy for a third of a second, entry after entry, and
//! every other request scheduled on that worker waited behind it.
//!
//! This pool takes it off the async workers. A job is a future (validation
//! awaits its terminology provider now and then); a pool thread drives it
//! with [`Handle::block_on`], so the CPU work happens on the pool thread and
//! only the I/O it waits for is served by the runtime. A Bundle's entries
//! are split into chunks that run on several threads at once.
//!
//! The pool is process-wide and its size is the bound on validation
//! parallelism: however many Bundles arrive together, at most
//! [`threads`] chunks are being validated; the rest wait in the queue, in
//! arrival order.
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `HFS_VALIDATION_THREADS` | half the CPUs (at least 1) | Pool size; `0` validates on the request's task, as before |
//! | `HFS_VALIDATION_CHUNK_SIZE` | 16–32, by Bundle size | Bundle entries validated per job |

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Condvar, Mutex, OnceLock};
use tokio::runtime::Handle;
use tracing::Instrument;

/// Stack of a pool thread (virtual reservation): the validator recurses, and
/// in a release build it does so on the calling thread.
const STACK: usize = 16 * 1024 * 1024;

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Pool {
    state: Mutex<State>,
    work: Condvar,
    limit: usize,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    threads: usize,
    idle: usize,
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// The pool size: `HFS_VALIDATION_THREADS`, else half the CPUs. Zero turns
/// the pool off.
pub(crate) fn threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        env_usize("HFS_VALIDATION_THREADS")
            .unwrap_or_else(|| {
                let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
                (cpus / 2).max(1)
            })
            .min(256)
    })
}

/// Entries per job for a Bundle of `entries` write entries: enough jobs to
/// use every thread, but 16 to 32 entries each — smaller jobs pay the
/// hand-over more often than it is worth, larger ones leave threads idle at
/// the end. `HFS_VALIDATION_CHUNK_SIZE` fixes it.
pub(crate) fn chunk_size(entries: usize) -> usize {
    static FIXED: OnceLock<Option<usize>> = OnceLock::new();
    if let Some(fixed) = FIXED.get_or_init(|| env_usize("HFS_VALIDATION_CHUNK_SIZE")) {
        return (*fixed).max(1);
    }
    entries.div_ceil(threads().max(1)).clamp(16, 32)
}

impl Pool {
    fn global() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        POOL.get_or_init(|| Pool {
            state: Mutex::default(),
            work: Condvar::new(),
            limit: threads(),
        })
    }

    /// Queue `job`; a thread is started when none is idle and the pool is
    /// not at its size yet. `Err` hands the job back when the pool has no
    /// thread at all and none could be started.
    fn submit(&'static self, job: Job) -> Result<(), Job> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.idle <= state.queue.len() && state.threads < self.limit {
            let spawned = std::thread::Builder::new()
                .name("hfs-validate".to_string())
                .stack_size(STACK)
                .spawn(move || self.run());
            match spawned {
                Ok(_) => state.threads += 1,
                Err(_) if state.threads == 0 => return Err(job),
                Err(_) => {}
            }
        }
        state.queue.push_back(job);
        drop(state);
        self.work.notify_one();
        Ok(())
    }

    fn run(&'static self) {
        loop {
            let job = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(job) = state.queue.pop_front() {
                        break job;
                    }
                    state.idle += 1;
                    state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
                    state.idle -= 1;
                }
            };
            // A job catches its own panic; the thread must outlive it.
            job();
        }
    }
}

/// Whether [`run`] hands work to the pool (`HFS_VALIDATION_THREADS` > 0).
pub(crate) fn enabled() -> bool {
    threads() > 0
}

/// Drive `future` to completion on a pool thread and return its output.
///
/// The future runs inside the calling task's runtime context (it may await
/// I/O and timers of that runtime, and spawn onto it) and inside the calling
/// span, but **not** inside the calling task: task-locals are not visible to
/// it, the caller re-establishes the ones it needs.
///
/// `None` when the future panicked (the panic is logged). When the pool is
/// off or has no thread, the future runs on the calling task instead.
pub(crate) async fn run<F, T>(future: F) -> Option<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    if !enabled() {
        return Some(future.await);
    }
    let handle = Handle::current();
    let span = tracing::Span::current();
    let (done, wait) = tokio::sync::oneshot::channel();
    // The future is parked here until a pool thread takes it, so a pool
    // without threads can hand it back.
    let slot = std::sync::Arc::new(Mutex::new(Some(future)));
    let taken = std::sync::Arc::clone(&slot);
    let job: Job = Box::new(move || {
        let Some(future) = taken.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.block_on(future.instrument(span))
        }));
        match outcome {
            Ok(output) => {
                let _ = done.send(output);
            }
            Err(panic) => {
                let detail = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                tracing::error!(panic = %detail, "validation job panicked");
            }
        }
    });
    if Pool::global().submit(job).is_err() {
        let future = slot.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        return Some(future.await);
    }
    wait.await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_are_16_to_32_entries() {
        if env_usize("HFS_VALIDATION_CHUNK_SIZE").is_some() {
            return;
        }
        assert_eq!(chunk_size(1), 16);
        assert_eq!(chunk_size(16), 16);
        assert_eq!(chunk_size(1_000_000), 32);
        let n = chunk_size(447);
        assert!((16..=32).contains(&n), "{n}");
    }

    /// The work runs off the calling thread, inside the caller's runtime:
    /// timers and spawned tasks work on a current-thread runtime too.
    #[tokio::test]
    async fn jobs_run_on_pool_threads_with_the_runtime_available() {
        if !enabled() {
            return;
        }
        let caller = std::thread::current().id();
        let (thread, name, spawned) = run(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let spawned = tokio::spawn(async { 21 * 2 }).await.unwrap();
            let current = std::thread::current();
            (current.id(), current.name().map(str::to_string), spawned)
        })
        .await
        .expect("job ran");
        assert_ne!(thread, caller);
        assert_eq!(name.as_deref(), Some("hfs-validate"));
        assert_eq!(spawned, 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn many_jobs_complete_and_a_panic_does_not_take_the_pool_down() {
        if !enabled() {
            return;
        }
        let panicked: Option<()> = run(async { panic!("boom (expected in this test)") }).await;
        assert!(panicked.is_none());
        let jobs = (0..200u64).map(|n| run(async move { n * n }));
        let results = futures::future::join_all(jobs).await;
        for (n, result) in results.into_iter().enumerate() {
            assert_eq!(result, Some((n * n) as u64));
        }
    }
}
