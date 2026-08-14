// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Retry/backoff helpers for GitHub API calls.

use reqwest::header::HeaderMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Retry decision returned for failed API responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryDecision {
    pub retryable: bool,
    pub delay: Duration,
}

impl RetryDecision {
    pub fn no_retry() -> Self {
        Self {
            retryable: false,
            delay: Duration::from_secs(0),
        }
    }
}

/// Determine retry strategy using status code and standard GitHub headers.
pub fn classify_retry(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    attempt: u32,
) -> RetryDecision {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return RetryDecision {
            retryable: true,
            delay: retry_delay_from_headers(headers).unwrap_or_else(|| exp_backoff(attempt)),
        };
    }

    if status == reqwest::StatusCode::FORBIDDEN && is_rate_limit_exhausted(headers) {
        return RetryDecision {
            retryable: true,
            delay: retry_delay_from_headers(headers).unwrap_or_else(|| exp_backoff(attempt)),
        };
    }

    if status.is_server_error() {
        return RetryDecision {
            retryable: true,
            delay: exp_backoff(attempt),
        };
    }

    RetryDecision::no_retry()
}

fn is_rate_limit_exhausted(headers: &HeaderMap) -> bool {
    headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|remaining| remaining.trim() == "0")
}

/// Exponential backoff with a capped delay.
pub fn exp_backoff(attempt: u32) -> Duration {
    let capped = attempt.min(7);
    let secs = 2u64.pow(capped);
    Duration::from_secs(secs.min(64))
}

fn retry_delay_from_headers(headers: &HeaderMap) -> Option<Duration> {
    if let Some(value) = headers.get("retry-after") {
        if let Ok(s) = value.to_str() {
            if let Ok(secs) = s.parse::<u64>() {
                return Some(Duration::from_secs(secs));
            }
        }
    }

    if let Some(value) = headers.get("x-ratelimit-reset") {
        if let Ok(s) = value.to_str() {
            if let Ok(epoch_secs) = s.parse::<u64>() {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if epoch_secs > now {
                    return Some(Duration::from_secs(epoch_secs - now));
                }
            }
        }
    }

    None
}
