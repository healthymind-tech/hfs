//! The dedicated `terminology-import` runtime (design §8.2 measure 1, U3).
//!
//! Terminology imports — `POST /tx/import` bodies, the admin job worker
//! (uploads, server-side imports, registry installs), bootstrap sync and the
//! post-bootstrap closure rebuild — run on a small multi-thread runtime of
//! their own instead of the one serving FHIR requests:
//!
//! * `HFS_TERMINOLOGY_IMPORT_THREADS` workers (default `clamp(cores/4, 1, 4)`);
//! * `max_blocking_threads = HFS_TERMINOLOGY_IMPORT_BLOCKING_THREADS` (4), so
//!   the importers' `spawn_blocking` parsing cannot take the whole machine;
//! * threads named `tx-import-*`, lowered to nice 10 on Linux, so the kernel
//!   prefers FHIR workers when both want a core.
//!
//! The runtime lives for the whole process (it is never dropped: dropping a
//! runtime inside another one panics, and the import jobs it runs are
//! long-lived anyway).

use std::sync::OnceLock;

/// Thread name prefix of the import runtime.
pub const THREAD_NAME: &str = "tx-import";

/// Niceness of import threads.
pub const IMPORT_NICE: i32 = 10;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Lower the calling thread's priority (Linux: per-thread `setpriority`).
fn lower_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain syscalls on the calling thread's own id; no memory is
    // shared with the kernel beyond the integer arguments.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid, IMPORT_NICE);
    }
}

/// Build an import runtime (tests build their own; production uses [`start`]).
pub fn build(workers: usize, blocking: usize) -> std::io::Result<tokio::runtime::Runtime> {
    let counter = std::sync::atomic::AtomicUsize::new(0);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers.max(1))
        .max_blocking_threads(blocking.max(1))
        .thread_name_fn(move || {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("{THREAD_NAME}-{n}")
        })
        .on_thread_start(lower_priority)
        .enable_all()
        .build()
}

/// Start (once) and return a handle to the process's import runtime.
pub fn start(workers: usize, blocking: usize) -> std::io::Result<tokio::runtime::Handle> {
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt.handle().clone());
    }
    let rt = build(workers, blocking)?;
    let handle = rt.handle().clone();
    if RUNTIME.set(rt).is_err() {
        // Lost a race: keep the winner, leak nothing that matters (the loser
        // has no tasks). Shut it down off-runtime.
        return Ok(RUNTIME.get().expect("set").handle().clone());
    }
    tracing::info!(
        workers,
        blocking,
        nice = IMPORT_NICE,
        "terminology import runtime started (tx-import-*)"
    );
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An import job's tasks — and the blocking parse work they spawn — run
    /// on `tx-import-*` threads, at lowered priority (design §8.2, U3).
    #[test]
    fn import_tasks_run_on_niced_tx_import_threads() {
        let rt = build(1, 2).unwrap();
        let (worker, blocking, nice) = rt.block_on(async {
            let handle = tokio::runtime::Handle::current();
            handle
                .spawn(async {
                    let worker = std::thread::current().name().map(str::to_string);
                    let (blocking, nice) = tokio::task::spawn_blocking(|| {
                        #[cfg(target_os = "linux")]
                        // SAFETY: reads the calling thread's own priority.
                        let nice = unsafe {
                            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
                            libc::getpriority(libc::PRIO_PROCESS, tid)
                        };
                        #[cfg(not(target_os = "linux"))]
                        let nice = IMPORT_NICE;
                        (std::thread::current().name().map(str::to_string), nice)
                    })
                    .await
                    .unwrap();
                    (worker, blocking, nice)
                })
                .await
                .unwrap()
        });
        assert!(worker.unwrap().starts_with("tx-import-"));
        assert!(blocking.unwrap().starts_with("tx-import-"));
        assert_eq!(nice, IMPORT_NICE);
        rt.shutdown_background();
    }

    /// A job handed over from the serving runtime runs on the import
    /// runtime and its result comes back.
    #[test]
    fn work_spawned_from_another_runtime_lands_on_import_threads() {
        let import = build(1, 1).unwrap();
        let handle = import.handle().clone();
        let serving = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let name = serving.block_on(async move {
            handle
                .spawn(async { std::thread::current().name().map(str::to_string) })
                .await
                .unwrap()
        });
        assert!(name.unwrap().starts_with("tx-import-"));
        import.shutdown_background();
    }
}
