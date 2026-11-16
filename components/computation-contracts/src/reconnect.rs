// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Reconnect and retry pacing shared by native plugins and their `*-common`
//! crates: exponential backoff with full jitter, bounded by a total time
//! budget and an optional attempt count.
//!
//! The policy holds no task or timer of its own: callers sleep for the delay
//! it returns, so dropping the caller's future (Stop with `Abort`, a deadline)
//! cancels the wait.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    time::Duration,
};
use tokio::time::Instant;

/// Bounds for one reconnect or retry sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackoffPolicy {
    /// Ceiling of the first delay; it doubles per attempt.
    pub initial: Duration,
    /// Largest delay ceiling.
    pub max: Duration,
    /// Total time from the first failure after which no attempt starts.
    pub budget: Duration,
    /// Most attempts, or `None` for as many as the budget allows.
    pub max_attempts: Option<u32>,
}

impl BackoffPolicy {
    /// A policy bounded only by `budget`, with the given delay ceilings.
    pub fn with_budget(initial: Duration, max: Duration, budget: Duration) -> Self {
        Self {
            initial,
            max,
            budget,
            max_attempts: None,
        }
    }

    /// Starts a sequence now.
    pub fn start(self) -> Backoff {
        Backoff {
            policy: self,
            started: Instant::now(),
            attempt: 0,
        }
    }
}

/// Progress through one reconnect or retry sequence.
#[derive(Debug)]
pub struct Backoff {
    policy: BackoffPolicy,
    started: Instant,
    attempt: u32,
}

impl Backoff {
    /// Attempts already scheduled.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The delay before the next attempt, or `None` once the attempts or the
    /// budget are spent. A delay that would end beyond the budget also ends
    /// the sequence.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self
            .policy
            .max_attempts
            .is_some_and(|max| self.attempt >= max)
        {
            return None;
        }
        let remaining = self.policy.budget.checked_sub(self.started.elapsed())?;
        let ceiling = self
            .policy
            .initial
            .saturating_mul(1u32 << self.attempt.min(16))
            .min(self.policy.max);
        let delay = jitter(ceiling);
        if delay >= remaining {
            return None;
        }
        self.attempt += 1;
        Some(delay)
    }

    /// Sleeps for the next delay; returns `false` once the sequence is over.
    pub async fn wait(&mut self) -> bool {
        match self.next_delay() {
            Some(delay) => {
                tokio::time::sleep(delay).await;
                true
            }
            None => false,
        }
    }
}

/// A uniformly random duration in `[0, ceiling]` ("full jitter"), so many
/// components retrying against one recovering server spread out.
pub fn jitter(ceiling: Duration) -> Duration {
    let random = RandomState::new().build_hasher().finish();
    let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(random % nanos.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_stay_below_a_doubling_ceiling_and_attempts_are_bounded() {
        let mut backoff = BackoffPolicy {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(40),
            budget: Duration::from_secs(600),
            max_attempts: Some(5),
        }
        .start();
        for ceiling in [10, 20, 40, 40, 40] {
            let delay = backoff.next_delay().expect("attempt");
            assert!(delay <= Duration::from_millis(ceiling), "{delay:?}");
        }
        assert!(backoff.next_delay().is_none());
        assert_eq!(backoff.attempt(), 5);
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_ends_the_sequence() {
        let mut backoff = BackoffPolicy::with_budget(
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(100),
        )
        .start();
        assert!(backoff.wait().await);
        tokio::time::advance(Duration::from_millis(101)).await;
        assert!(!backoff.wait().await);
        assert!(BackoffPolicy::with_budget(
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::ZERO
        )
        .start()
        .next_delay()
        .is_none());
    }
}
