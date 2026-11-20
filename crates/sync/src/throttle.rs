//! Request pacing towards external servers: a token bucket
//! (`maxRequestsPerSecond`) and retry/backoff honouring `Retry-After`.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

/// Token bucket: `rate` tokens per second, bursts up to `rate`.
#[derive(Debug)]
pub struct TokenBucket {
    rate: f64,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    /// A bucket for `per_second` requests (min 1).
    pub fn new(per_second: u32) -> Self {
        let rate = f64::from(per_second.max(1));
        Self {
            rate,
            state: Mutex::new((rate, Instant::now())),
        }
    }

    /// How long a caller must wait for a token taken at `now`, consuming it.
    /// Pure arithmetic over the bucket state (unit-testable).
    fn reserve(rate: f64, tokens: &mut f64, last: &mut Instant, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(*last).as_secs_f64();
        *tokens = (*tokens + elapsed * rate).min(rate);
        *last = now;
        *tokens -= 1.0;
        if *tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-*tokens / rate)
        }
    }

    /// Waits for a token.
    pub async fn acquire(&self) {
        let wait = {
            let mut st = self.state.lock().await;
            let (ref mut tokens, ref mut last) = *st;
            Self::reserve(self.rate, tokens, last, Instant::now())
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// Retry policy for transient failures.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    /// Attempts in total (first try included).
    pub max_attempts: u32,
    /// Delay before the first retry.
    pub base: Duration,
    /// Upper bound of any single delay (including `Retry-After`).
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base: Duration::from_millis(500),
            max: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// Delay before retry number `attempt` (1-based): exponential from
    /// `base`, capped, or the server's `Retry-After` when it gave one.
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let d = match retry_after {
            Some(d) => d,
            None => self
                .base
                .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1).min(16))),
        };
        d.min(self.max)
    }

    /// Whether another attempt follows attempt number `attempt` (1-based).
    pub fn should_retry(&self, attempt: u32) -> bool {
        attempt < self.max_attempts
    }
}

/// Whether an HTTP status is worth retrying.
pub fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Parses `Retry-After`: delta seconds or an HTTP date.
pub fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = DateTime::parse_from_rfc2822(v).ok()?.with_timezone(&Utc);
    Some((at - now).to_std().unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_arithmetic() {
        let t0 = Instant::now();
        let (mut tokens, mut last) = (2.0, t0);
        assert_eq!(
            TokenBucket::reserve(2.0, &mut tokens, &mut last, t0),
            Duration::ZERO
        );
        assert_eq!(
            TokenBucket::reserve(2.0, &mut tokens, &mut last, t0),
            Duration::ZERO
        );
        // Third request in the same instant waits half a second.
        let w = TokenBucket::reserve(2.0, &mut tokens, &mut last, t0);
        assert!((w.as_secs_f64() - 0.5).abs() < 1e-9, "{w:?}");
        // After a second, refilled (capped at the rate).
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(
            TokenBucket::reserve(2.0, &mut tokens, &mut last, t1),
            Duration::ZERO
        );
        assert!(tokens <= 2.0);
    }

    #[tokio::test]
    async fn acquire_paces() {
        let b = TokenBucket::new(50);
        let start = Instant::now();
        for _ in 0..55 {
            b.acquire().await;
        }
        // 5 requests beyond the burst at 50/s ≈ 100 ms.
        assert!(start.elapsed() >= Duration::from_millis(80));
        let _ = TokenBucket::new(0); // min 1
    }

    #[test]
    fn backoff_delays() {
        let b = Backoff::default();
        assert_eq!(b.delay(1, None), Duration::from_millis(500));
        assert_eq!(b.delay(3, None), Duration::from_secs(2));
        assert_eq!(b.delay(30, None), Duration::from_secs(60));
        assert_eq!(
            b.delay(1, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
        assert_eq!(
            b.delay(1, Some(Duration::from_secs(700))),
            Duration::from_secs(60)
        );
        assert!(b.should_retry(4));
        assert!(!b.should_retry(5));
    }

    #[test]
    fn statuses_and_retry_after() {
        assert!(retryable_status(429) && retryable_status(503));
        assert!(!retryable_status(400) && !retryable_status(412));
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(parse_retry_after(" 3 ", now), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 2026 00:00:10 GMT", now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            parse_retry_after("Wed, 31 Dec 2025 00:00:10 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }
}
