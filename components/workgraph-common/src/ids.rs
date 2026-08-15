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

//! Deterministic WorkGraph identifiers.
//!
//! ```text
//! bodyDigest = "sha256:" + lowerHex(sha256(utf8(body ?? "")))
//! runId      = "validation:" + projectItemNodeId + ":" + bodyDigest
//! eventId    = "event:" + runId + ":" + eventType
//! executionId = "execution:" + runId
//! ```
//!
//! `runId` deliberately does not include `subjectNodeId`. GitHub's Project Item
//! node ID already identifies the subject binding, while `bodyDigest` identifies
//! the exact validation content.

use sha2::{Digest, Sha256};

use crate::event::{EventId, RunId, Sha256Digest, WorkGraphEventType};

/// Return whether `value` is exactly `PVTI_[A-Za-z0-9]+`.
pub fn is_valid_project_item_node_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("PVTI_") else {
        return false;
    };
    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Digest the authoritative issue body (`body ?? ""`) as `sha256:<64-hex>`.
///
/// The input must be the exact body string GitHub reports for the issue; no
/// trimming, newline normalization, or Markdown processing is applied.
pub fn body_digest(body: Option<&str>) -> Sha256Digest {
    let digest = sha256_hex(body.unwrap_or(""));
    Sha256Digest::from_hex(&digest).expect("sha256 hex is always a valid digest")
}

/// Derive the deterministic `runId` for one Project Item and content digest.
pub fn run_id(project_item_node_id: &str, content_digest: &Sha256Digest) -> RunId {
    RunId::new(project_item_node_id, content_digest)
        .expect("run_id inputs must satisfy the WorkGraph identity grammar")
}

/// Derive the deterministic `eventId` for one logical event within a run.
pub fn event_id(run_id: &RunId, event_type: WorkGraphEventType) -> EventId {
    EventId::new(run_id, event_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEM: &str = "PVTI_example";
    const DIGEST: &str = "sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa";

    fn digest() -> Sha256Digest {
        Sha256Digest::try_from(DIGEST.to_string()).expect("valid digest")
    }

    #[test]
    fn empty_and_missing_bodies_digest_identically() {
        let empty = body_digest(Some(""));
        let missing = body_digest(None);
        assert_eq!(empty, missing);
        assert_eq!(
            empty.as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn body_digest_matches_reference_vector() {
        assert_eq!(
            body_digest(Some("Context\nWorkGraph-Validation: pass\n")).as_str(),
            DIGEST
        );
    }

    #[test]
    fn project_item_node_id_grammar_is_exact() {
        assert!(is_valid_project_item_node_id("PVTI_example"));
        assert!(is_valid_project_item_node_id("PVTI_A09z"));
        for invalid in [
            "PVT_example",
            "PVTI_",
            "PVTI_with-dash",
            "PVTI_with_underscore",
            "PVTI_with space",
        ] {
            assert!(!is_valid_project_item_node_id(invalid), "{invalid}");
        }
    }

    #[test]
    fn readable_identifiers_match_reference_vector() {
        let run = run_id(ITEM, &digest());
        assert_eq!(run.as_str(), format!("validation:{ITEM}:{DIGEST}"));
        assert_eq!(
            event_id(&run, WorkGraphEventType::CompletedIssueValidation).as_str(),
            format!("event:validation:{ITEM}:{DIGEST}:CompletedIssueValidation")
        );
    }

    #[test]
    fn run_id_changes_only_with_project_item_or_digest() {
        let base = run_id(ITEM, &digest());
        let other_digest = body_digest(Some("different"));
        assert_ne!(base, run_id("PVTI_other", &digest()));
        assert_ne!(base, run_id(ITEM, &other_digest));
        assert_eq!(base, run_id(ITEM, &digest()));
    }

    #[test]
    fn event_id_is_unique_per_event_type_within_a_run() {
        let run = run_id(ITEM, &digest());
        let ids = WorkGraphEventType::ALL.map(|event_type| event_id(&run, event_type));
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "event ids {i} and {j} collided");
            }
        }
    }
}
