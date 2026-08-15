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

//! Human summary lines for WorkGraph comments.
//!
//! Summaries are generated here, from values the caller has already verified
//! against authoritative GitHub metadata, and are never parsed back into
//! meaning. No component reads a summary to make a decision; routing,
//! idempotency, and trust all derive from the JSON object and from GitHub's own
//! immutable metadata.
//!
//! Every function in this module returns a string that satisfies
//! [`crate::comment::validate_summary`], so rendering can never fail because of
//! a generated summary.

use crate::comment::MAX_SUMMARY_CHARS;
use crate::event::{WorkGraphEvent, WorkGraphEventPayload};

/// The verified subject a summary refers to.
///
/// Both fields must come from authoritative GitHub Source metadata (or a graph
/// relation to it) — never from the event JSON, which does not carry them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubjectRef<'a> {
    /// `owner/repo` for the subject issue.
    pub repository: &'a str,
    /// The subject issue number.
    pub number: u64,
}

impl std::fmt::Display for SubjectRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.repository, self.number)
    }
}

/// Generate the canonical summary line for an event.
pub fn summary_for(event: &WorkGraphEvent, subject: SubjectRef<'_>) -> String {
    let subject = sanitize(&subject.to_string());
    let raw = match &event.payload {
        WorkGraphEventPayload::ResponsibilityAssigned(_) => {
            format!("WorkGraph assigned issue validation for {subject}")
        }
        WorkGraphEventPayload::ExecutionStarted(_) => {
            format!("WorkGraph started the issue validation agent for {subject}")
        }
        WorkGraphEventPayload::CompletedIssueValidation(payload) => format!(
            "WorkGraph issue validation {} for {subject}",
            payload.outcome.as_str()
        ),
        WorkGraphEventPayload::RoutingDecided(payload) => format!(
            "WorkGraph routed {subject} to {}",
            payload.to_status.as_str()
        ),
    };
    clamp(&raw)
}

/// Collapse whitespace and drop every character a summary may not carry.
///
/// This uses exactly [`crate::comment::is_forbidden_summary_char`], so a
/// generated summary always satisfies [`crate::comment::validate_summary`] even
/// when the verified subject it is built from carries hostile characters.
fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if crate::comment::is_forbidden_summary_char(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Trim to a single line of at most [`MAX_SUMMARY_CHARS`] characters.
fn clamp(value: &str) -> String {
    let sanitized = sanitize(value);
    if sanitized.is_empty() {
        return "WorkGraph event".to_string();
    }
    if sanitized.chars().count() <= MAX_SUMMARY_CHARS {
        return sanitized;
    }
    let mut clamped: String = sanitized.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    clamped.push('…');
    clamped.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::validate_summary;
    use crate::event::{
        AssignedResponsibilityType, CompletedIssueValidationPayload, ExecutionId,
        ExecutionStartedPayload, ProfileRef, ResponsibilityAssignedPayload, RoutingDecidedPayload,
        ValidationOutcome, ValidationReasonCode,
    };
    use crate::ids::{body_digest, run_id};

    const ITEM: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";
    const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";

    fn event(payload: WorkGraphEventPayload) -> WorkGraphEvent {
        WorkGraphEvent::new(
            run_id(ITEM, SUBJECT, &body_digest(Some("body"))),
            ITEM,
            SUBJECT,
            payload,
        )
        .expect("valid event")
    }

    fn subject() -> SubjectRef<'static> {
        SubjectRef {
            repository: "drasi-project/drasi-core",
            number: 742,
        }
    }

    #[test]
    fn summaries_are_specific_and_valid() {
        let cases = [
            (
                WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
                    responsibility_type: AssignedResponsibilityType::IssueValidation,
                    profile_ref: ProfileRef::new("issue-validator", BLOB).expect("valid"),
                    content_digest: body_digest(Some("body")),
                }),
                "WorkGraph assigned issue validation for drasi-project/drasi-core#742",
            ),
            (
                WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                    execution_id: ExecutionId::from_suffix("abc").expect("valid"),
                    task_id: "task-1".to_string(),
                }),
                "WorkGraph started the issue validation agent for drasi-project/drasi-core#742",
            ),
            (
                WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                    execution_id: ExecutionId::from_suffix("abc").expect("valid"),
                    outcome: ValidationOutcome::Failed,
                    reason_code: ValidationReasonCode::RequiredMarkerMissing,
                }),
                "WorkGraph issue validation failed for drasi-project/drasi-core#742",
            ),
            (
                WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
                    ValidationOutcome::Failed,
                )),
                "WorkGraph routed drasi-project/drasi-core#742 to NeedsMoreInformation",
            ),
        ];

        for (payload, expected) in cases {
            let summary = summary_for(&event(payload), subject());
            assert_eq!(summary, expected);
            validate_summary(&summary).expect("generated summaries are always renderable");
        }
    }

    #[test]
    fn long_subjects_are_clamped_to_a_renderable_summary() {
        let repository = "o".repeat(300);
        let summary = summary_for(
            &event(WorkGraphEventPayload::RoutingDecided(
                RoutingDecidedPayload::for_outcome(ValidationOutcome::Passed),
            )),
            SubjectRef {
                repository: &repository,
                number: 1,
            },
        );
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS);
        validate_summary(&summary).expect("clamped summaries stay renderable");
    }

    #[test]
    fn hostile_subjects_still_produce_renderable_summaries() {
        // A repository name is authoritative metadata, but nothing stops it (or
        // a future subject field) from containing line separators or bidi
        // overrides; a generated summary must still be renderable.
        let summary = summary_for(
            &event(WorkGraphEventPayload::RoutingDecided(
                RoutingDecidedPayload::for_outcome(ValidationOutcome::Passed),
            )),
            SubjectRef {
                repository: "owner/re\u{2028}po\u{202E}\u{FEFF}x",
                number: 9,
            },
        );
        validate_summary(&summary).expect("sanitized summaries stay renderable");
        assert!(!summary.contains('\u{2028}'));
        assert!(!summary.contains('\u{202E}'));
        assert!(!summary.contains('\u{FEFF}'));
    }

    #[test]
    fn control_characters_are_stripped() {
        let summary = summary_for(
            &event(WorkGraphEventPayload::RoutingDecided(
                RoutingDecidedPayload::for_outcome(ValidationOutcome::Passed),
            )),
            SubjectRef {
                repository: "owner/re\npo\t",
                number: 9,
            },
        );
        validate_summary(&summary).expect("sanitized summaries stay renderable");
        assert!(summary.contains("owner/re po"), "unexpected: {summary}");
    }
}
