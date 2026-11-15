// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bounded exponential backoff with full jitter for one operation. The wait
//! holds no state outside its future, so the host cancelling the in-flight
//! `handle` (pause, Abort, a deadline) ends it immediately.

use std::{
    hash::{BuildHasher, Hasher},
    time::Duration,
};
use tokio::time::Instant;

/// First backoff ceiling; doubles per attempt up to [`MAX_BACKOFF`].
pub const BASE_BACKOFF: Duration = Duration::from_millis(100);
/// Largest backoff ceiling between attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);

pub(crate) struct Backoff {
    budget: Duration,
    started: Instant,
    attempt: u32,
}

impl Backoff {
    pub fn new(budget: Duration) -> Self {
        Self {
            budget,
            started: Instant::now(),
            attempt: 0,
        }
    }

    /// The next delay, or `None` once the budget would be exceeded.
    pub fn next_delay(&mut self) -> Option<Duration> {
        let remaining = self.budget.checked_sub(self.started.elapsed())?;
        let ceiling = BASE_BACKOFF
            .saturating_mul(1u32 << self.attempt.min(16))
            .min(MAX_BACKOFF);
        let delay = jitter(ceiling);
        if delay >= remaining {
            return None;
        }
        self.attempt += 1;
        Some(delay)
    }

    /// Sleeps for the next delay; `false` when retrying is over.
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

fn jitter(ceiling: Duration) -> Duration {
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(random % nanos.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_budget_never_retries_and_delays_stay_within_the_ceiling() {
        assert!(Backoff::new(Duration::ZERO).next_delay().is_none());
        let mut backoff = Backoff::new(Duration::from_secs(3600));
        for attempt in 0..20u32 {
            let delay = backoff.next_delay().expect("budget remains");
            let ceiling = BASE_BACKOFF
                .saturating_mul(1u32 << attempt.min(16))
                .min(MAX_BACKOFF);
            assert!(delay <= ceiling);
        }
    }
}
