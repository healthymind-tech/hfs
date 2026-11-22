//! Operation permits for terminology calls made from FHIR request paths
//! (design §8.2 measure 3, package U3).
//!
//! [`PermittedTerminology`] wraps any [`TerminologyService`] with a
//! semaphore (`HFS_TERMINOLOGY_OPERATION_PERMITS`, default 64). Validation,
//! `:in` search, SQL-on-FHIR and FHIRPath reach terminology through it, so a
//! burst of FHIR traffic cannot pile an unbounded number of concurrent
//! expansions onto the embedded service (and its PostgreSQL pool). A call
//! that cannot get a permit waits up to its own deadline and then fails with
//! [`TxError::Unavailable`], which each caller maps onto its existing failure
//! policy (`HFS_SEARCH_TERMINOLOGY_FAIL`, `HFS_VALIDATION_TERMINOLOGY_FAIL`).
//!
//! The `/tx` API used by external clients does not go through permits: it
//! has the request timeout and the anonymous rate limit instead.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::service::{
    ServiceKind, TerminologyHealth, TerminologyService, TxError, TxRequest, TxResponse,
};

/// A [`TerminologyService`] that admits at most `permits` concurrent calls.
pub struct PermittedTerminology<T: ?Sized> {
    inner: Arc<T>,
    permits: Arc<Semaphore>,
    capacity: usize,
    default_timeout: Duration,
}

impl<T: TerminologyService + ?Sized> PermittedTerminology<T> {
    /// `permits` of zero means unlimited (no semaphore is consulted).
    pub fn new(inner: Arc<T>, permits: usize, default_timeout: Duration) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(permits.max(1))),
            capacity: permits,
            default_timeout,
        }
    }

    /// Permits not currently held (`terminology_permits_available`).
    pub fn available(&self) -> usize {
        if self.capacity == 0 {
            return usize::MAX;
        }
        self.permits.available_permits()
    }

    /// Configured capacity (0 = unlimited).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The wrapped service.
    pub fn inner(&self) -> &Arc<T> {
        &self.inner
    }
}

#[async_trait]
impl<T: TerminologyService + ?Sized> TerminologyService for PermittedTerminology<T> {
    fn kind(&self) -> ServiceKind {
        self.inner.kind()
    }

    async fn call(&self, request: TxRequest) -> Result<TxResponse, TxError> {
        if self.capacity == 0 {
            return self.inner.call(request).await;
        }
        let deadline = request.timeout.unwrap_or(self.default_timeout);
        let started = tokio::time::Instant::now();
        let permit = match tokio::time::timeout(deadline, self.permits.clone().acquire_owned())
            .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(TxError::Unavailable("terminology permits closed".into())),
            Err(_) => {
                return Err(TxError::Unavailable(format!(
                    "terminology overloaded: no operation permit within {deadline:?} \
                     (HFS_TERMINOLOGY_OPERATION_PERMITS={})",
                    self.capacity
                )));
            }
        };
        // The time spent queueing counts against the caller's deadline.
        let mut request = request;
        let remaining = deadline.saturating_sub(started.elapsed());
        request.timeout = Some(remaining.max(Duration::from_millis(1)));
        let result = self.inner.call(request).await;
        drop(permit);
        result
    }

    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.inner.changes()
    }

    async fn health(&self) -> TerminologyHealth {
        // Health bypasses permits: a saturated service is still up.
        self.inner.health().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Holds every call for `hold`, counting the peak concurrency.
    struct Slow {
        hold: Duration,
        now: AtomicUsize,
        peak: AtomicUsize,
    }

    #[async_trait]
    impl TerminologyService for Slow {
        fn kind(&self) -> ServiceKind {
            ServiceKind::Embedded
        }
        async fn call(&self, _request: TxRequest) -> Result<TxResponse, TxError> {
            let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(self.hold).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            Ok(TxResponse {
                status: 200,
                content_type: None,
                body: Bytes::from_static(b"{}"),
            })
        }
    }

    fn slow(hold: Duration) -> Arc<Slow> {
        Arc::new(Slow {
            hold,
            now: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn caps_concurrency() {
        let inner = slow(Duration::from_millis(50));
        let tx = Arc::new(PermittedTerminology::new(
            inner.clone(),
            3,
            Duration::from_secs(10),
        ));
        let calls: Vec<_> = (0..12)
            .map(|_| {
                let tx = tx.clone();
                tokio::spawn(async move { tx.call(TxRequest::get("metadata")).await })
            })
            .collect();
        for c in futures_join(calls).await {
            assert!(c.is_ok());
        }
        assert_eq!(inner.peak.load(Ordering::SeqCst), 3);
        assert_eq!(tx.available(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_call_fails_unavailable_at_its_deadline() {
        let inner = slow(Duration::from_secs(5));
        let tx = Arc::new(PermittedTerminology::new(inner, 1, Duration::from_secs(10)));
        let busy = {
            let tx = tx.clone();
            tokio::spawn(async move { tx.call(TxRequest::get("metadata")).await })
        };
        tokio::task::yield_now().await;
        let err = tx
            .call(TxRequest::get("metadata").with_timeout(Duration::from_millis(200)))
            .await
            .unwrap_err();
        assert!(matches!(err, TxError::Unavailable(_)), "{err:?}");
        assert!(err.is_transient());
        assert!(busy.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn zero_means_unlimited() {
        let inner = slow(Duration::from_millis(1));
        let tx = PermittedTerminology::new(inner, 0, Duration::from_secs(1));
        assert_eq!(tx.available(), usize::MAX);
        assert!(tx.call(TxRequest::get("metadata")).await.is_ok());
    }

    async fn futures_join<I, T>(handles: I) -> Vec<T>
    where
        I: IntoIterator<Item = tokio::task::JoinHandle<T>>,
    {
        let mut out = Vec::new();
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }
}
