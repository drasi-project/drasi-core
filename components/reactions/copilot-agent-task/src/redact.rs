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

//! Helpers to keep secrets (PAT/App tokens) out of logs.
//!
//! # Contract
//!
//! * The GitHub token is **never** logged, printed via `Debug`, or embedded in
//!   error messages returned up the stack. [`GitHubClient`](crate::github::GitHubClient)'s
//!   `Debug` impl always prints `"[REDACTED]"` for the token field.

/// Redact a bearer/basic `Authorization` header value for inclusion in a log
/// line or error message, keeping only the auth scheme (e.g. `Bearer`).
pub fn redact_authorization(value: &str) -> String {
    match value.split_once(' ') {
        Some((scheme, _)) => format!("{scheme} [REDACTED]"),
        None => "[REDACTED]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_authorization_keeps_only_scheme() {
        assert_eq!(
            redact_authorization("Bearer ghp_supersecrettoken"),
            "Bearer [REDACTED]"
        );
        assert_eq!(redact_authorization("garbage"), "[REDACTED]");
    }
}
