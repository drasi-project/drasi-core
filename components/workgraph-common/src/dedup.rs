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

//! Duplicate coalescing and conflict detection for logical WorkGraph events.
//!
//! A logical event is identified by its deterministic `eventId`. A *physical*
//! observation of that event is one GitHub issue comment, identified by its
//! immutable comment node ID. The two together are the provenance and
//! idempotency key:
//!
//! * the deterministic `eventId` says *which* logical event this is, so a
//!   retried write is recognised as the same event rather than a new one; and
//! * the comment node ID says *which physical write* produced it, so recovery
//!   can point at a specific comment as evidence.
//!
//! Two observations of the same `eventId` whose canonical event JSON is
//! byte-identical are duplicates and coalesce to one. Two observations of the
//! same `eventId` whose canonical event JSON differs are a contradiction that
//! no consumer can safely resolve, so they fail closed.
//!
//! Summaries are deliberately excluded from the comparison: they are generated
//! prose, are never consumed for routing, and must not be able to wedge a run.
//!
//! # Reading versus adopting
//!
//! [`coalesce`] answers "which comment carries event X?" and is what a reader
//! of *someone else's* event uses.
//!
//! [`adopt_published_event`] answers the different question a **writer** asks
//! after an ambiguous write: "is the event already published byte-for-byte the
//! event I am about to publish?". The `eventId` is a deterministic hash of the
//! run and the event type only — it does **not** cover the payload — so a
//! single already-published comment can carry the intended `eventId` with a
//! different payload. Adopting it would silently substitute someone else's
//! content for the reaction's own intent, so it fails closed instead.

use crate::comment::WorkGraphComment;
use crate::event::{EventId, WorkGraphEvent};

/// One physical observation of a WorkGraph event comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedComment {
    /// The immutable GitHub comment node ID that carried the event.
    pub comment_node_id: String,
    /// The parsed, validated comment.
    pub comment: WorkGraphComment,
}

impl ObservedComment {
    /// The logical event identifier this observation carries.
    pub fn event_id(&self) -> &EventId {
        &self.comment.event.event_id
    }
}

/// Failure modes when reducing observations to a single accepted event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DuplicateError {
    /// Two comments claimed the same event ID with different content.
    #[error(
        "event '{event_id}' is claimed with conflicting content by comments '{first}' and '{second}'"
    )]
    Conflict {
        /// The contested event ID.
        event_id: String,
        /// The first (accepted-order) comment node ID.
        first: String,
        /// The conflicting comment node ID.
        second: String,
    },
    /// An already-published comment carries the intended event ID but not the
    /// intended content, so it cannot stand in for the intended write.
    #[error(
        "event '{event_id}' was already published by comment '{comment}' with content that \
         differs from the event this reaction intends to publish; refusing to adopt it"
    )]
    Divergent {
        /// The intended event ID.
        event_id: String,
        /// The already-published comment node ID.
        comment: String,
    },
}

/// Reduce every observation of `event_id` to at most one accepted comment.
///
/// Returns `Ok(None)` when no observation carries `event_id`, `Ok(Some(_))`
/// with the first observation in input order when all matching observations
/// agree, and [`DuplicateError::Conflict`] when any two disagree.
///
/// Input order should be GitHub's comment order, so the accepted observation is
/// the earliest physical write.
pub fn coalesce<'a>(
    observed: &'a [ObservedComment],
    event_id: &EventId,
) -> Result<Option<&'a ObservedComment>, DuplicateError> {
    let mut accepted: Option<&ObservedComment> = None;
    let mut accepted_json = String::new();

    for observation in observed.iter().filter(|o| o.event_id() == event_id) {
        let json = observation.comment.event.to_canonical_json();
        match accepted {
            None => {
                accepted_json = json;
                accepted = Some(observation);
            }
            Some(first) if accepted_json != json => {
                return Err(DuplicateError::Conflict {
                    event_id: event_id.to_string(),
                    first: first.comment_node_id.clone(),
                    second: observation.comment_node_id.clone(),
                });
            }
            Some(_) => {}
        }
    }
    Ok(accepted)
}

/// Adopt an already-published observation of the event a writer intends to
/// publish, or fail closed.
///
/// This is the **only** safe way for a reaction to treat a pre-existing comment
/// as its own completed write. A deterministic `eventId` covers the run and the
/// event type but **not** the payload, so an observation carrying the intended
/// `eventId` is adoptable only when its canonical event JSON — envelope *and*
/// payload — equals the canonical JSON of `intended` exactly.
///
/// Returns:
///
/// * `Ok(None)` — nothing has published this event yet, so the caller must
///   write it;
/// * `Ok(Some(_))` — the earliest observation that is byte-identical to
///   `intended` (summaries may differ, since they are non-authoritative); and
/// * `Err(_)` — a single divergent observation ([`DuplicateError::Divergent`])
///   or two contradictory ones ([`DuplicateError::Conflict`]). Either way the
///   caller must halt with no further side effect: the reaction's intent and
///   the published state disagree, and only an operator can resolve that.
pub fn adopt_published_event<'a>(
    observed: &'a [ObservedComment],
    intended: &WorkGraphEvent,
) -> Result<Option<&'a ObservedComment>, DuplicateError> {
    let intended_json = intended.to_canonical_json();
    let mut accepted: Option<&ObservedComment> = None;

    for observation in observed
        .iter()
        .filter(|o| o.event_id() == &intended.event_id)
    {
        let json = observation.comment.event.to_canonical_json();
        if json != intended_json {
            return Err(match accepted {
                // Two published comments disagree with each other as well as
                // with the intent; report the contradiction that is visible in
                // the issue thread.
                Some(first) => DuplicateError::Conflict {
                    event_id: intended.event_id.to_string(),
                    first: first.comment_node_id.clone(),
                    second: observation.comment_node_id.clone(),
                },
                None => DuplicateError::Divergent {
                    event_id: intended.event_id.to_string(),
                    comment: observation.comment_node_id.clone(),
                },
            });
        }
        if accepted.is_none() {
            accepted = Some(observation);
        }
    }
    Ok(accepted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::parse_comment;
    use crate::event::{
        CompletedIssueValidationPayload, ExecutionId, ValidationOutcome, ValidationReasonCode,
        WorkGraphEvent, WorkGraphEventPayload, WorkGraphEventType,
    };
    use crate::ids::{body_digest, event_id, run_id};
    use crate::summary::summary_for;

    const ITEM: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";

    fn completion(outcome: ValidationOutcome) -> WorkGraphEvent {
        let reason = match outcome {
            ValidationOutcome::Passed => ValidationReasonCode::RequiredMarkerPresent,
            ValidationOutcome::Failed => ValidationReasonCode::RequiredMarkerMissing,
        };
        let run = run_id(ITEM, &body_digest(Some("body")));
        WorkGraphEvent::new(
            run.clone(),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                execution_id: ExecutionId::from_run_id(&run),
                outcome,
                reason_code: reason,
            }),
        )
        .expect("valid event")
    }

    fn observed(node_id: &str, event: &WorkGraphEvent) -> ObservedComment {
        let body =
            crate::comment::render_comment(event, &summary_for(event)).expect("render canonical");
        ObservedComment {
            comment_node_id: node_id.to_string(),
            comment: parse_comment(&body).expect("parse"),
        }
    }

    #[test]
    fn identical_duplicates_coalesce_to_the_earliest_comment() {
        let event = completion(ValidationOutcome::Passed);
        let summary = summary_for(&event);
        let observations = vec![observed("IC_first", &event), observed("IC_second", &event)];
        assert_eq!(summary, "Issue validation passed.");
        let accepted = coalesce(&observations, &event.event_id)
            .expect("identical duplicates coalesce")
            .expect("one accepted");
        assert_eq!(accepted.comment_node_id, "IC_first");
    }

    #[test]
    fn misleading_summaries_fail_before_coalescing() {
        let event = completion(ValidationOutcome::Passed);
        let canonical =
            crate::comment::render_comment(&event, &summary_for(&event)).expect("render");
        let misleading =
            canonical.replacen("Issue validation passed.", "Issue validation failed.", 1);
        assert!(matches!(
            parse_comment(&misleading).expect_err("misleading summary rejected"),
            crate::comment::CommentError::SummaryMismatch { .. }
        ));
    }

    #[test]
    fn conflicting_content_for_one_event_id_fails_closed() {
        let passed = completion(ValidationOutcome::Passed);
        let failed = completion(ValidationOutcome::Failed);
        // Same run, same event type, therefore the same deterministic event ID.
        assert_eq!(passed.event_id, failed.event_id);
        let observations = vec![
            observed("IC_first", &passed),
            observed("IC_second", &failed),
        ];
        let error = coalesce(&observations, &passed.event_id).expect_err("conflict must fail");
        assert_eq!(
            error,
            DuplicateError::Conflict {
                event_id: passed.event_id.to_string(),
                first: "IC_first".to_string(),
                second: "IC_second".to_string(),
            }
        );
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let event = completion(ValidationOutcome::Passed);
        let observations = vec![observed("IC_first", &event)];
        let other = event_id(&event.run_id, WorkGraphEventType::RoutingDecided);
        assert!(coalesce(&observations, &other)
            .expect("no conflict")
            .is_none());
    }

    #[test]
    fn adoption_requires_the_exact_intended_payload() {
        let intended = completion(ValidationOutcome::Passed);

        // Nothing published yet.
        assert!(adopt_published_event(&[], &intended)
            .expect("nothing to adopt")
            .is_none());

        // Byte-identical canonical comments are adoptable.
        let observations = vec![
            observed("IC_first", &intended),
            observed("IC_second", &intended),
        ];
        let accepted = adopt_published_event(&observations, &intended)
            .expect("identical duplicates adopt")
            .expect("one accepted");
        assert_eq!(accepted.comment_node_id, "IC_first");
    }

    #[test]
    fn a_single_divergent_published_event_fails_closed() {
        // The eventId hashes the run and the event type only, so a *different*
        // payload can carry the very same eventId.
        let intended = completion(ValidationOutcome::Passed);
        let divergent = completion(ValidationOutcome::Failed);
        assert_eq!(intended.event_id, divergent.event_id);
        assert_ne!(
            intended.to_canonical_json(),
            divergent.to_canonical_json(),
            "the payloads must differ for this test to mean anything"
        );

        let observations = vec![observed("IC_divergent", &divergent)];
        // `coalesce` would happily hand back the single divergent comment...
        assert_eq!(
            coalesce(&observations, &intended.event_id)
                .expect("no conflict among one comment")
                .expect("one observation")
                .comment_node_id,
            "IC_divergent"
        );
        // ...but a writer must never adopt it as its own published event.
        assert_eq!(
            adopt_published_event(&observations, &intended).expect_err("divergence fails closed"),
            DuplicateError::Divergent {
                event_id: intended.event_id.to_string(),
                comment: "IC_divergent".to_string(),
            }
        );
    }

    #[test]
    fn two_published_comments_that_disagree_report_a_conflict() {
        let intended = completion(ValidationOutcome::Passed);
        let divergent = completion(ValidationOutcome::Failed);
        let observations = vec![
            observed("IC_first", &intended),
            observed("IC_second", &divergent),
        ];
        assert_eq!(
            adopt_published_event(&observations, &intended).expect_err("conflict fails closed"),
            DuplicateError::Conflict {
                event_id: intended.event_id.to_string(),
                first: "IC_first".to_string(),
                second: "IC_second".to_string(),
            }
        );
    }

    #[test]
    fn adoption_ignores_observations_of_other_events() {
        let intended = completion(ValidationOutcome::Passed);
        let run = run_id(ITEM, &body_digest(Some("a different body")));
        let other_run = WorkGraphEvent::new(
            run.clone(),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                execution_id: ExecutionId::from_run_id(&run),
                outcome: ValidationOutcome::Failed,
                reason_code: ValidationReasonCode::RequiredMarkerMissing,
            }),
        )
        .expect("valid event");
        assert_ne!(intended.event_id, other_run.event_id);

        let observations = vec![observed("IC_other", &other_run)];
        assert!(adopt_published_event(&observations, &intended)
            .expect("unrelated events are ignored")
            .is_none());
    }
}
