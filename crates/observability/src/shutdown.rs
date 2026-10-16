//! The stop signal every Helios server drains on.
//!
//! Ctrl-C (SIGINT) is what a developer sends from a terminal; SIGTERM is what a
//! process supervisor, container runtime, or load-balanced rolling deploy
//! sends. Both must drain in-flight requests the same way. SIGTERM matters more
//! than it looks: the container image `exec`s the server as PID 1, and the
//! kernel discards a signal sent to PID 1 unless the process installed a
//! handler for it — so a server that only listens for Ctrl-C ignores
//! `docker stop` entirely and dies to the SIGKILL at the end of the grace
//! period.
//!
//! ```rust,ignore
//! axum::serve(listener, app)
//!     .with_graceful_shutdown(async {
//!         let signal = helios_observability::shutdown::signal().await;
//!         tracing::info!(signal, "Shutdown signal received, draining connections");
//!     })
//!     .await?;
//! // Flush here, after the drain -- not inside the future above.
//! helios_observability::telemetry::shutdown();
//! ```
//!
//! Do the flushing *after* `serve` returns. axum awaits the whole shutdown
//! future before it stops accepting connections or tells any of them to wind
//! down, so work placed inside it runs while requests are still in flight and
//! misses everything they record.

/// Resolves when the process is asked to stop, returning the signal's name
/// (`"SIGINT"` or `"SIGTERM"`) for the shutdown log line.
///
/// If the SIGTERM handler cannot be installed this warns and keeps Ctrl-C
/// only. Non-Unix targets listen for Ctrl-C only.
pub async fn signal() -> &'static str {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to install the SIGTERM handler; only Ctrl-C will stop this instance"
                );
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => "SIGINT",
        _ = terminate => "SIGTERM",
    }
}

/// How long a stopping server may take to drain before [`StopGuard`] ends
/// the process, when the `*_SHUTDOWN_TIMEOUT` variable is unset: under the
/// 10 s `docker stop` grace period, so the process exits on its own rather
/// than to the `SIGKILL` that follows it.
pub const DEFAULT_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(8);

/// Parses a `*_SHUTDOWN_TIMEOUT` value in whole seconds; unset, empty or
/// malformed gives `default`.
pub fn parse_grace(value: Option<&str>, default: std::time::Duration) -> std::time::Duration {
    match value.map(str::trim) {
        None | Some("") => default,
        Some(raw) => raw
            .parse::<u64>()
            .map(std::time::Duration::from_secs)
            .unwrap_or_else(|_| {
                tracing::warn!(value = raw, "shutdown timeout is not a whole number of seconds; using the default");
                default
            }),
    }
}

/// Makes a stop signal end the process at every stage, not only while the
/// HTTP server is draining.
///
/// [`signal`] is normally awaited only once the server is listening. Before
/// that — loading the database, seeding SearchParameters, rehydrating
/// profiles, building terminology indexes, which takes minutes on a large
/// or slow database — a server running as PID 1 in a container has no
/// handler installed, the kernel discards `SIGTERM`, and `docker stop` waits
/// out its grace period and kills it. And once draining, a request or
/// background job that never finishes keeps the process (and its port)
/// alive indefinitely.
///
/// The guard listens from the moment it is created:
///
/// * a signal before [`StopGuard::serving`] exits at once (status 0) — the
///   startup work is all re-done on the next start;
/// * a signal after it leaves the drain to the server, but exits (status 1)
///   when the drain takes longer than `grace`, or at a second signal.
///
/// Create it first thing in `main`, inside the runtime.
#[derive(Clone)]
pub struct StopGuard {
    serving: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl StopGuard {
    /// Starts listening. `server` names the process in the log lines.
    pub fn install(server: &'static str, grace: std::time::Duration) -> Self {
        let serving = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&serving);
        tokio::spawn(async move {
            let first = signal().await;
            if !flag.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::warn!(
                    signal = first,
                    server,
                    "Stop signal received while starting up; exiting without serving"
                );
                crate::telemetry::shutdown();
                std::process::exit(0);
            }
            let outcome = tokio::select! {
                _ = tokio::time::sleep(grace) => "did not finish in time",
                second = signal() => second,
            };
            tracing::warn!(
                signal = first,
                server,
                grace_secs = grace.as_secs(),
                outcome,
                "Shutdown still draining; exiting now (open requests and background jobs are \
                 abandoned)"
            );
            crate::telemetry::shutdown();
            std::process::exit(1);
        });
        Self { serving }
    }

    /// The server is about to listen: from now on a signal starts a drain
    /// instead of ending the process at once.
    pub fn serving(&self) {
        self.serving
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod grace_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn grace_parses_whole_seconds_with_a_default() {
        let d = Duration::from_secs(8);
        assert_eq!(parse_grace(None, d), d);
        assert_eq!(parse_grace(Some(" "), d), d);
        assert_eq!(parse_grace(Some("3"), d), Duration::from_secs(3));
        assert_eq!(parse_grace(Some("0"), d), Duration::ZERO);
        assert_eq!(parse_grace(Some("soon"), d), d);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A SIGTERM delivered to this process resolves [`signal`] as `"SIGTERM"`
    /// instead of terminating the test binary — i.e. the handler is really
    /// installed. Raised only after the future has been polled once, because
    /// the handler is registered on first poll.
    #[tokio::test]
    async fn sigterm_resolves_the_signal_future() {
        let mut fut = std::pin::pin!(signal());
        assert!(futures_poll_once(fut.as_mut()).await.is_none());
        // SAFETY: `raise` only sends a signal to this process.
        unsafe {
            libc_raise(15);
        }
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .expect("signal() did not resolve after SIGTERM");
        assert_eq!(got, "SIGTERM");
    }

    async fn futures_poll_once<F: std::future::Future + Unpin>(fut: F) -> Option<F::Output> {
        tokio::select! {
            biased;
            out = fut => Some(out),
            _ = tokio::task::yield_now() => None,
        }
    }

    unsafe extern "C" {
        #[link_name = "raise"]
        fn libc_raise(sig: i32) -> i32;
    }
}
