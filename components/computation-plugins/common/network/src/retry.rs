// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Delivery retry policy for network sinks.
//!
//! Retries use exponential backoff with full jitter, honour a delay the
//! destination requests (HTTP `Retry-After`), and stop when either the
//! attempt count or the total time budget is exhausted. A retry loop holds no
//! state outside its future, so the host cancelling an in-flight `handle`
//! (Stop with `Abort`, or a deadline) drops it immediately, including any
//! backoff sleep; a Drain stop waits at most for the remaining budget.

use std::{
    hash::{BuildHasher, Hasher},
    time::Duration,
};
use tokio::time::Instant;

/// First backoff ceiling; doubles per attempt up to [`MAX_BACKOFF`].
pub const BASE_BACKOFF: Duration = Duration::from_millis(100);
/// Largest backoff ceiling between attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Upper bound on a configured retry budget.
pub const MAX_RETRY_BUDGET_MS: u64 = 3_600_000;
/// Default total time allowed for retries of one delivery, in milliseconds.
pub const DEFAULT_RETRY_BUDGET_MS: u64 = 120_000;

/// Bounds for retrying one delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Additional attempts after the first.
    pub max_retries: u32,
    /// Total time from the first attempt after which no retry starts.
    pub budget: Duration,
}

/// Progress through one delivery's retries.
#[derive(Debug)]
pub struct Retry {
    policy: RetryPolicy,
    started: Instant,
    attempt: u32,
}

impl Retry {
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            policy,
            started: Instant::now(),
            attempt: 0,
        }
    }

    /// The number of retries already scheduled.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The delay before the next attempt, or `None` when the attempts or the
    /// budget are exhausted. A `requested` delay from the destination replaces
    /// the jittered backoff; one that would exceed the budget ends retrying.
    pub fn next_delay(&mut self, requested: Option<Duration>) -> Option<Duration> {
        if self.attempt >= self.policy.max_retries {
            return None;
        }
        let remaining = self.policy.budget.checked_sub(self.started.elapsed())?;
        let delay = match requested {
            Some(requested) => requested,
            None => jitter(backoff_ceiling(self.attempt)),
        };
        if delay >= remaining {
            return None;
        }
        self.attempt += 1;
        Some(delay)
    }

    /// Sleeps for the next delay; returns `false` when retrying is over.
    pub async fn wait(&mut self, requested: Option<Duration>) -> bool {
        match self.next_delay(requested) {
            Some(delay) => {
                tokio::time::sleep(delay).await;
                true
            }
            None => false,
        }
    }
}

fn backoff_ceiling(attempt: u32) -> Duration {
    BASE_BACKOFF
        .saturating_mul(1u32 << attempt.min(16))
        .min(MAX_BACKOFF)
}

/// A uniformly random duration in `[0, ceiling]` ("full jitter"), so many
/// sinks retrying against one recovering destination spread out.
fn jitter(ceiling: Duration) -> Duration {
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(random % nanos.saturating_add(1))
}

/// Parses an HTTP `Retry-After` value: delay-seconds or an HTTP date.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let delay = date.signed_duration_since(chrono::Utc::now());
    Some(delay.to_std().unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_retries: u32, budget_ms: u64) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            budget: Duration::from_millis(budget_ms),
        }
    }

    #[test]
    fn backoff_is_jittered_below_a_doubling_ceiling() {
        let mut retry = Retry::new(policy(10, 600_000));
        for attempt in 0..10 {
            let delay = retry.next_delay(None).expect("retry");
            assert!(delay <= backoff_ceiling(attempt), "{delay:?}");
        }
        assert!(retry.next_delay(None).is_none(), "attempts exhausted");
        assert_eq!(backoff_ceiling(20), MAX_BACKOFF);
    }

    #[test]
    fn requested_delays_are_honoured_within_the_budget() {
        let mut retry = Retry::new(policy(5, 10_000));
        assert_eq!(
            retry.next_delay(Some(Duration::from_secs(2))),
            Some(Duration::from_secs(2))
        );
        assert!(
            retry.next_delay(Some(Duration::from_secs(60))).is_none(),
            "a requested delay beyond the budget ends retrying"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_bounds_total_retry_time() {
        let mut retry = Retry::new(policy(100, 1_000));
        tokio::time::advance(Duration::from_millis(1_001)).await;
        assert!(retry.next_delay(None).is_none());
        assert_eq!(retry.attempt(), 0);
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates() {
        assert_eq!(parse_retry_after(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon"), None);
    }
}
