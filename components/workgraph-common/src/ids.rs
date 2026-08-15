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
//! Every identifier in this module is a pure function of authoritative GitHub
//! values, so any participant (Rust reaction or JavaScript reporter) that sees
//! the same authoritative inputs derives the same identifier without
//! coordination. The algorithms are frozen; changing any preimage below is a
//! breaking change to `workgraph.event/v1`.
//!
//! # Algorithms
//!
//! All hashes are SHA-256 over UTF-8 bytes, rendered as lowercase hex.
//!
//! ```text
//! bodyDigest = "sha256:" || hex(sha256(body ?? ""))
//!
//! runId      = "run:sha256:"   || hex(sha256(
//!                  "workgraph.run/v1"      || LF ||
//!                  projectItemNodeId       || LF ||
//!                  subjectNodeId           || LF ||
//!                  bodyDigest))
//!
//! eventId    = "event:sha256:" || hex(sha256(
//!                  "workgraph.event/v1"    || LF ||
//!                  runId                   || LF ||
//!                  eventType))
//! ```
//!
//! `LF` is a single `\n` (0x0A). `bodyDigest` is embedded **with** its
//! `sha256:` prefix, and `runId` is embedded **with** its `run:sha256:`
//! prefix. `eventType` is the exact serialized event-type token (for example
//! `CompletedIssueValidation`).
//!
//! The textual forms are exactly what a Cypher continuous query builds with the
//! generic `sha256(text)` scalar and string concatenation:
//!
//! ```cypher
//! runHex  = sha256('workgraph.run/v1\n' + item.nodeId + '\n' + issue.nodeId + '\n' + issue.bodyDigest)
//! runId   = 'run:sha256:' + runHex
//! eventId = 'event:sha256:' + sha256('workgraph.event/v1\n' + runId + '\nResponsibilityAssigned')
//! ```
//!
//! `body` is the authoritative issue body as returned by GitHub. A missing
//! body (`null`) and an empty body both digest as the empty string, which is
//! why the contract is written `body ?? ""`.
//!
//! # Uniqueness properties
//!
//! * `runId` identifies exactly one validation of one exact issue body for one
//!   Project Item. Editing the issue body produces a new `runId`; re-adding the
//!   same Project Item does not.
//! * `eventId` identifies exactly one logical event within a run. Two
//!   physically distinct comments carrying the same `eventId` are duplicates:
//!   byte-identical payloads coalesce, conflicting payloads fail closed.
//!
//! See `vectors/workgraph-event-v1.vectors.json` for cross-language test
//! vectors.

use sha2::{Digest, Sha256};

use crate::event::{EventId, RunId, Sha256Digest, WorkGraphEventType};

/// Domain separator for [`run_id`].
pub const RUN_ID_DOMAIN: &str = "workgraph.run/v1";
/// Domain separator for [`event_id`].
pub const EVENT_ID_DOMAIN: &str = "workgraph.event/v1";

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

/// Derive the deterministic `runId` for one validation of one exact issue body
/// on one Project Item.
pub fn run_id(
    project_item_node_id: &str,
    subject_node_id: &str,
    body_digest: &Sha256Digest,
) -> RunId {
    let preimage = format!(
        "{RUN_ID_DOMAIN}\n{project_item_node_id}\n{subject_node_id}\n{}",
        body_digest.as_str()
    );
    RunId::from_hex(&sha256_hex(&preimage)).expect("sha256 hex is always a valid run id")
}

/// Derive the deterministic `eventId` for one logical event within a run.
pub fn event_id(run_id: &RunId, event_type: WorkGraphEventType) -> EventId {
    let preimage = format!(
        "{EVENT_ID_DOMAIN}\n{}\n{}",
        run_id.as_str(),
        event_type.as_str()
    );
    EventId::from_hex(&sha256_hex(&preimage)).expect("sha256 hex is always a valid event id")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEM: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";

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
        // Independently reproducible: `printf 'Ready for validation.\n' | shasum -a 256`.
        assert_eq!(
            body_digest(Some("Ready for validation.\n")).as_str(),
            "sha256:9f33654f064aa70964905f069948a69a331895913a888b43d636c4f6caa29cf2"
        );
    }

    #[test]
    fn run_id_changes_with_every_input() {
        let digest = body_digest(Some("body"));
        let other = body_digest(Some("body edited"));
        let base = run_id(ITEM, SUBJECT, &digest);
        assert_ne!(base, run_id("PVTI_other", SUBJECT, &digest));
        assert_ne!(base, run_id(ITEM, "I_other", &digest));
        assert_ne!(base, run_id(ITEM, SUBJECT, &other));
        assert_eq!(base, run_id(ITEM, SUBJECT, &digest));
    }

    #[test]
    fn event_id_is_unique_per_event_type_within_a_run() {
        let run = run_id(ITEM, SUBJECT, &body_digest(Some("body")));
        let ids = [
            event_id(&run, WorkGraphEventType::ResponsibilityAssigned),
            event_id(&run, WorkGraphEventType::ExecutionStarted),
            event_id(&run, WorkGraphEventType::CompletedIssueValidation),
            event_id(&run, WorkGraphEventType::RoutingDecided),
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "event ids {i} and {j} collided");
            }
        }
        assert_eq!(
            ids[0],
            event_id(&run, WorkGraphEventType::ResponsibilityAssigned)
        );
    }

    #[test]
    fn identifiers_use_their_declared_prefixes() {
        let run = run_id(ITEM, SUBJECT, &body_digest(None));
        assert!(run.as_str().starts_with("run:sha256:"));
        assert_eq!(run.as_str().len(), "run:sha256:".len() + 64);
        let event = event_id(&run, WorkGraphEventType::RoutingDecided);
        assert!(event.as_str().starts_with("event:sha256:"));
        assert_eq!(event.as_str().len(), "event:sha256:".len() + 64);
    }
}
