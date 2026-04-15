//! Simple async token-bucket rate limiter.
//!
//! The bucket refills at `rate_per_second` tokens/s up to a maximum of
//! `burst` tokens.  Each API call costs one token; if no token is available
//! the caller `await`s until one becomes available.
//!
//! # Example
//! ```rust,no_run
//! use genebears::rate_limiter::RateLimiter;
//! use std::time::Duration;
//!
//! // Allow 5 requests/s with a burst of 10.
//! let rl = RateLimiter::new(5.0, 10);
//! // In async context:
//! // rl.acquire().await;
//! ```

use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Token-bucket rate limiter (thread-safe, async-compatible).
pub struct RateLimiter {
    inner: Mutex<Bucket>,
}

struct Bucket {
    /// Maximum tokens the bucket can hold.
    capacity: f64,
    /// Current token count (fractional tokens are fine internally).
    tokens: f64,
    /// Tokens added per second.
    rate: f64,
    /// When the bucket was last refilled.
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a new limiter.
    ///
    /// * `rate_per_second` — sustained request rate (e.g. `5.0` → 5 req/s)
    /// * `burst` — maximum tokens that can accumulate (≥ 1)
    pub fn new(rate_per_second: f64, burst: u32) -> Self {
        assert!(rate_per_second > 0.0, "rate must be positive");
        assert!(burst >= 1, "burst must be at least 1");
        RateLimiter {
            inner: Mutex::new(Bucket {
                capacity: burst as f64,
                tokens: burst as f64, // start full
                rate: rate_per_second,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Acquire one token, sleeping until one is available.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut b = self.inner.lock().unwrap();
                // Refill based on elapsed time.
                let now = Instant::now();
                let elapsed = now.duration_since(b.last_refill).as_secs_f64();
                b.tokens = (b.tokens + elapsed * b.rate).min(b.capacity);
                b.last_refill = now;

                if b.tokens >= 1.0 {
                    b.tokens -= 1.0;
                    None // token acquired, no wait needed
                } else {
                    // How long until we have a full token?
                    let secs_needed = (1.0 - b.tokens) / b.rate;
                    Some(Duration::from_secs_f64(secs_needed))
                }
            };

            match wait {
                None => return,
                Some(dur) => sleep(dur).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use tokio::runtime::Runtime;

    fn rt() -> Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn burst_tokens_acquired_instantly() {
        let rl = RateLimiter::new(1.0, 5);
        let start = Instant::now();

        rt().block_on(async {
            for _ in 0..5 {
                rl.acquire().await;
            }
        });

        assert!(
            start.elapsed() < Duration::from_millis(100),
            "burst tokens should be available instantly, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn throttled_after_burst_exhausted() {
        let rl = RateLimiter::new(10.0, 2);

        let start = Instant::now();
        rt().block_on(async {
            rl.acquire().await;
            rl.acquire().await;
            rl.acquire().await;
        });

        assert!(
            start.elapsed() >= Duration::from_millis(80),
            "expected throttling after burst, got {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn single_acquire_on_fresh_limiter_is_instant() {
        let rl = RateLimiter::new(1.0, 1);
        let start = Instant::now();
        rt().block_on(async { rl.acquire().await });
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn rate_limits_correctly() {
        let rl = RateLimiter::new(5.0, 1);

        let start = Instant::now();
        rt().block_on(async {
            rl.acquire().await;
            rl.acquire().await;
        });

        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(150),
            "expected ~200 ms wait, got {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(600),
            "waited too long: {:?}",
            elapsed
        );
    }
}
