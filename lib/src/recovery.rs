// Copyright 2025 The Drasi Authors.
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

//! Recovery policy and error types for checkpoint-based recovery.
//!
//! See the design doc at
//! <https://github.com/drasi-project/design-documents/tree/main/drasi-lib/Source-Checkpoints>
//! (doc 03 §4 — Recovery Policies) for the semantic rationale.

use serde::{Deserialize, Serialize};

/// Behavior when a source cannot honor a requested resume position.
///
/// Configured per-query via `QueryConfig::recovery_policy`. When the field is
/// `None`, a future global default applies (which itself defaults to `Strict`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicy {
    /// Fail startup if the source cannot honor the requested resume position
    /// (e.g., WAL pruned past the checkpoint, replication slot invalidated).
    /// Requires manual intervention. Favors correctness over availability.
    #[default]
    Strict,
    /// Automatically wipe the query's persistent index and perform a full
    /// re-bootstrap on gap detection. Favors availability over consistency.
    AutoReset,
}

/// Errors specific to checkpoint-based recovery.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// A persistent query requires all of its sources to support positional
    /// replay, but one or more do not (e.g., transient HTTP source with WAL
    /// disabled).
    #[error("Incompatible source '{source_id}' for persistent query '{query_id}': {reason}")]
    IncompatibleSource {
        query_id: String,
        source_id: String,
        reason: String,
    },

    /// The `AutoReset` recovery policy fired: the query's index has been
    /// wiped and a full bootstrap will follow.
    #[error("Auto-reset triggered for query '{query_id}': {reason}")]
    AutoResetTriggered { query_id: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_strict() {
        assert_eq!(RecoveryPolicy::default(), RecoveryPolicy::Strict);
    }

    #[test]
    fn json_round_trip_strict() {
        let json = serde_json::to_string(&RecoveryPolicy::Strict).unwrap();
        assert_eq!(json, "\"strict\"");
        let back: RecoveryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RecoveryPolicy::Strict);
    }

    #[test]
    fn json_round_trip_auto_reset() {
        let json = serde_json::to_string(&RecoveryPolicy::AutoReset).unwrap();
        assert_eq!(json, "\"auto_reset\"");
        let back: RecoveryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RecoveryPolicy::AutoReset);
    }

    #[test]
    fn yaml_round_trip() {
        for policy in [RecoveryPolicy::Strict, RecoveryPolicy::AutoReset] {
            let yaml = serde_yaml::to_string(&policy).unwrap();
            let back: RecoveryPolicy = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(back, policy);
        }
    }

    #[test]
    fn incompatible_source_message() {
        let err = RecoveryError::IncompatibleSource {
            query_id: "q1".into(),
            source_id: "s1".into(),
            reason: "no WAL".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("q1"));
        assert!(msg.contains("s1"));
        assert!(msg.contains("no WAL"));
    }

    #[test]
    fn auto_reset_message() {
        let err = RecoveryError::AutoResetTriggered {
            query_id: "q1".into(),
            reason: "gap".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("q1"));
        assert!(msg.contains("gap"));
    }
}
