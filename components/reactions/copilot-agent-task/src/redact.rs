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

//! Helpers to keep secrets (PAT/App tokens) and prompt bodies out of logs.
//!
//! # Contract
//!
//! * The GitHub token is **never** logged, printed via `Debug`, or embedded in
//!   error messages returned up the stack. [`GitHubClient`](crate::github::GitHubClient)'s
//!   `Debug` impl always prints `"[REDACTED]"` for the token field.
//! * Prompt text (which embeds repository/issue content) is only logged as a
//!   short, length-bounded preview — never in full — via [`preview`].

/// Maximum characters kept by [`preview`] before truncation.
const PREVIEW_LEN: usize = 120;

/// Produce a short, log-safe preview of a potentially large or sensitive
/// string (e.g. a rendered prompt). Truncates at a character boundary and
/// appends an ellipsis marker so it is visually obvious the value was cut.
pub fn preview(s: &str) -> String {
    if s.chars().count() <= PREVIEW_LEN {
        return s.to_string();
    }
    let truncated: String = s.chars().take(PREVIEW_LEN).collect();
    format!("{truncated}… [{} chars total]", s.chars().count())
}

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
    fn preview_passes_short_strings_through() {
        assert_eq!(preview("short"), "short");
    }

    #[test]
    fn preview_truncates_long_strings() {
        let long = "a".repeat(500);
        let out = preview(&long);
        assert!(out.len() < long.len());
        assert!(out.contains("500 chars total"));
    }

    #[test]
    fn redact_authorization_keeps_only_scheme() {
        assert_eq!(
            redact_authorization("Bearer ghp_supersecrettoken"),
            "Bearer [REDACTED]"
        );
        assert_eq!(redact_authorization("garbage"), "[REDACTED]");
    }
}
