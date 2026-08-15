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

//! The outer WorkGraph issue-comment grammar.
//!
//! A WorkGraph comment body is exactly:
//!
//! ```text
//! WorkGraphEvent/v1<LF><LF><summary><LF><LF><json>
//! ```
//!
//! * The first line is exactly `WorkGraphEvent/v1`.
//! * The second line is empty.
//! * The third line is one non-empty human summary of at most 120 characters,
//!   with no leading/trailing whitespace and no character that could make it
//!   read as something other than what it is — no control characters, no
//!   line/paragraph separators, and no bidirectional formatting marks (see
//!   [`is_forbidden_summary_char`]).
//! * The fourth line is empty.
//! * The remainder is one raw JSON object that ends at end-of-comment.
//!
//! There is no Markdown fence, no prologue, and no epilogue. `\r\n` is
//! normalized to `\n` before parsing, because GitHub web submissions arrive
//! CRLF-encoded.
//!
//! The summary is generated from already-verified values (see
//! [`crate::summary`]) and exists purely for humans reading the issue thread.
//! **Nothing ever routes on it.** Only the JSON object is authoritative, and
//! only after it parses into a fully validated [`WorkGraphEvent`].
//!
//! # Legacy bodies
//!
//! Pure-JSON comments and fenced ```` ```json ```` comments are **not**
//! accepted and have no migration path: a pure-JSON body is reported as
//! [`CommentError::NotWorkGraphEvent`] (silently ignorable, since it never
//! claimed to be a WorkGraph event) and a fenced body is reported as
//! [`CommentError::MissingBlankLineAfterMarker`]. Neither ever yields an event.

use crate::event::{EventError, WorkGraphEvent};

/// The exact first line of every WorkGraph event comment.
pub const COMMENT_MARKER: &str = "WorkGraphEvent/v1";

/// The maximum number of characters allowed in the human summary line.
pub const MAX_SUMMARY_CHARS: usize = 120;

/// Errors produced while rendering or parsing a WorkGraph comment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommentError {
    /// The body does not begin with the `WorkGraphEvent/v1` marker line.
    ///
    /// Callers should treat this as "not a WorkGraph comment" and ignore the
    /// comment without logging an error.
    #[error("comment is not a '{COMMENT_MARKER}' comment")]
    NotWorkGraphEvent,
    /// The marker line was present but was not followed by an empty line.
    #[error("'{COMMENT_MARKER}' marker must be followed by an empty line")]
    MissingBlankLineAfterMarker,
    /// The summary line was not followed by an empty line and a JSON object.
    #[error("summary line must be followed by an empty line and one JSON object")]
    MissingBlankLineAfterSummary,
    /// The summary line was empty.
    #[error("summary must be a non-empty single line")]
    EmptySummary,
    /// The summary spanned more than one line.
    #[error("summary must be a single line")]
    MultilineSummary,
    /// The summary exceeded [`MAX_SUMMARY_CHARS`].
    #[error("summary must be at most {MAX_SUMMARY_CHARS} characters, got {0}")]
    SummaryTooLong(usize),
    /// The summary carried a forbidden character or surrounding whitespace.
    #[error(
        "summary must not contain control, line-separator, or bidirectional formatting characters, \
         nor leading/trailing whitespace"
    )]
    UnnormalizedSummary,
    /// The comment ended after the summary with no JSON object.
    #[error("comment must end with one JSON object")]
    MissingEvent,
    /// Content followed the JSON object, or the JSON did not start an object.
    #[error("comment must contain exactly one JSON object ending at end-of-comment")]
    TrailingText,
    /// The JSON object was malformed.
    #[error("event JSON is malformed: {0}")]
    InvalidJson(String),
    /// The JSON object was not a valid `workgraph.event/v1` document.
    #[error(transparent)]
    InvalidEvent(#[from] EventError),
}

/// A parsed WorkGraph comment body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkGraphComment {
    /// The human summary line, exactly as written.
    pub summary: String,
    /// The validated event carried by the comment.
    pub event: WorkGraphEvent,
}

impl WorkGraphComment {
    /// Re-render this comment in canonical form.
    ///
    /// Round-tripping a foreign producer's body through
    /// [`parse_comment`] and back yields the canonical byte sequence, which is
    /// what duplicate/conflict detection compares.
    pub fn to_canonical_body(&self) -> String {
        render_comment(&self.event, &self.summary)
            .expect("a parsed comment always re-renders successfully")
    }
}

/// Normalize CRLF line endings to LF.
///
/// Applied to every body before parsing; GitHub returns CRLF for comments
/// authored through the web UI and LF for comments created through the API.
pub fn normalize_line_endings(body: &str) -> String {
    body.replace("\r\n", "\n")
}

/// Whether a body claims to be a WorkGraph event comment.
///
/// Use this to distinguish "ignore quietly" (`false`) from "a WorkGraph comment
/// that failed strict parsing" (`true`) when [`parse_comment`] returns an error.
pub fn is_workgraph_comment(body: &str) -> bool {
    normalize_line_endings(body)
        .split('\n')
        .next()
        .is_some_and(|first| first == COMMENT_MARKER)
}

/// Whether a character must never appear in a summary line.
///
/// [`char::is_control`] covers only the `Cc` category, which would still let a
/// summary smuggle a line break (U+2028 / U+2029) or silently reorder itself
/// for a human reader (bidirectional overrides and isolates, zero-width marks,
/// a byte-order mark). Nothing ever routes on a summary, but it is the one part
/// of a WorkGraph comment a person reads, so it must say exactly what it looks
/// like it says.
pub fn is_forbidden_summary_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{2028}' | '\u{2029}'                 // line / paragraph separator
            | '\u{FEFF}'                            // zero-width no-break space (BOM)
            | '\u{200E}' | '\u{200F}'               // left-to-right / right-to-left mark
            | '\u{202A}'..='\u{202E}'               // bidi embeddings and overrides
            | '\u{2066}'..='\u{2069}'               // bidi isolates
        )
}

/// Validate a human summary line against the grammar.
pub fn validate_summary(summary: &str) -> Result<(), CommentError> {
    if summary.is_empty() {
        return Err(CommentError::EmptySummary);
    }
    if summary.contains('\n') {
        return Err(CommentError::MultilineSummary);
    }
    if summary.chars().any(is_forbidden_summary_char) || summary.trim() != summary {
        return Err(CommentError::UnnormalizedSummary);
    }
    let length = summary.chars().count();
    if length > MAX_SUMMARY_CHARS {
        return Err(CommentError::SummaryTooLong(length));
    }
    Ok(())
}

/// Render one event and one summary as a canonical comment body.
pub fn render_comment(event: &WorkGraphEvent, summary: &str) -> Result<String, CommentError> {
    validate_summary(summary)?;
    event.validate()?;
    Ok(format!(
        "{COMMENT_MARKER}\n\n{summary}\n\n{}",
        event.to_canonical_json()
    ))
}

/// Strictly parse a comment body into a summary and a validated event.
pub fn parse_comment(body: &str) -> Result<WorkGraphComment, CommentError> {
    let normalized = normalize_line_endings(body);

    let Some(after_marker) = normalized.strip_prefix(COMMENT_MARKER) else {
        return Err(CommentError::NotWorkGraphEvent);
    };
    // The marker must be a whole line, so `WorkGraphEvent/v1x` is not a
    // malformed WorkGraph comment — it is not a WorkGraph comment at all.
    if !(after_marker.is_empty() || after_marker.starts_with('\n')) {
        return Err(CommentError::NotWorkGraphEvent);
    }
    let Some(after_header) = after_marker.strip_prefix("\n\n") else {
        return Err(CommentError::MissingBlankLineAfterMarker);
    };

    let Some((summary, json)) = after_header.split_once("\n\n") else {
        return Err(CommentError::MissingBlankLineAfterSummary);
    };
    validate_summary(summary)?;

    if json.is_empty() {
        return Err(CommentError::MissingEvent);
    }
    if !json.starts_with('{') || json.trim_end() != json {
        return Err(CommentError::TrailingText);
    }
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
        // serde_json reports content after a complete value as trailing
        // characters; surface that as the grammar violation it is.
        if error.to_string().contains("trailing characters") {
            CommentError::TrailingText
        } else {
            CommentError::InvalidJson(error.to_string())
        }
    })?;

    let event = WorkGraphEvent::from_value(value)?;
    Ok(WorkGraphComment {
        summary: summary.to_string(),
        event,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        AssignedResponsibilityType, CompletedIssueValidationPayload, ExecutionId,
        ExecutionStartedPayload, ProfileRef, ResponsibilityAssignedPayload, RoutingDecidedPayload,
        ValidationOutcome, ValidationReasonCode, WorkGraphEventPayload,
    };
    use crate::ids::{body_digest, run_id};

    const ITEM: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";
    const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";
    const BODY: &str = "Please validate. workgraph:validate";

    fn event(payload: WorkGraphEventPayload) -> WorkGraphEvent {
        WorkGraphEvent::new(
            run_id(ITEM, &body_digest(Some(BODY))),
            ITEM,
            SUBJECT,
            payload,
        )
        .expect("valid event")
    }

    fn all_payloads() -> Vec<WorkGraphEventPayload> {
        let execution_id = ExecutionId::from_run_id(&run_id(ITEM, &body_digest(Some(BODY))));
        vec![
            WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
                responsibility_type: AssignedResponsibilityType::IssueValidation,
                profile_ref: ProfileRef::new("issue-validator", BLOB).expect("valid"),
                content_digest: body_digest(Some(BODY)),
            }),
            WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                execution_id: execution_id.clone(),
                task_id: "task-42".to_string(),
            }),
            WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                execution_id,
                outcome: ValidationOutcome::Passed,
                reason_code: ValidationReasonCode::RequiredMarkerPresent,
            }),
            WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
                ValidationOutcome::Passed,
            )),
        ]
    }

    #[test]
    fn every_event_type_round_trips() {
        for payload in all_payloads() {
            let event = event(payload);
            let summary = format!("WorkGraph {} for owner/repo#1", event.event_type());
            let body = render_comment(&event, &summary).expect("render");
            let parsed = parse_comment(&body).expect("parse");
            assert_eq!(parsed.event, event);
            assert_eq!(parsed.summary, summary);
            assert_eq!(parsed.to_canonical_body(), body);
        }
    }

    #[test]
    fn rendered_body_matches_the_exact_grammar() {
        let event = event(all_payloads().remove(1));
        let body = render_comment(&event, "Issue validation started.").expect("render");
        let lines: Vec<&str> = body.split('\n').collect();
        assert_eq!(lines[0], COMMENT_MARKER);
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "Issue validation started.");
        assert_eq!(lines[3], "");
        assert!(lines[4].starts_with('{'));
        assert!(body.ends_with('}'));
        assert!(!body.contains("```"));
        assert_eq!(lines.len(), 5, "JSON must be a single trailing line");
    }

    #[test]
    fn crlf_bodies_parse() {
        let event = event(all_payloads().remove(0));
        let body = render_comment(&event, "Issue validation assigned.").expect("render");
        let crlf = body.replace('\n', "\r\n");
        assert_eq!(parse_comment(&crlf).expect("parse crlf").event, event);
    }

    #[test]
    fn legacy_pure_json_is_not_a_workgraph_comment() {
        let json = event(all_payloads().remove(0)).to_canonical_json();
        assert!(!is_workgraph_comment(&json));
        assert_eq!(
            parse_comment(&json).expect_err("legacy json ignored"),
            CommentError::NotWorkGraphEvent
        );
    }

    #[test]
    fn legacy_fenced_body_is_rejected() {
        let json = event(all_payloads().remove(0)).to_canonical_json();
        let legacy = format!("{COMMENT_MARKER}\n```json\n{json}\n```");
        assert!(is_workgraph_comment(&legacy));
        assert_eq!(
            parse_comment(&legacy).expect_err("fenced body rejected"),
            CommentError::MissingBlankLineAfterMarker
        );
    }

    #[test]
    fn fenced_body_inside_the_json_slot_is_rejected() {
        let json = event(all_payloads().remove(0)).to_canonical_json();
        let legacy = format!("{COMMENT_MARKER}\n\nSummary line\n\n```json\n{json}\n```");
        assert_eq!(
            parse_comment(&legacy).expect_err("fenced json slot rejected"),
            CommentError::TrailingText
        );
    }

    #[test]
    fn marker_must_be_a_whole_line() {
        assert_eq!(
            parse_comment("WorkGraphEvent/v10\n\nSummary\n\n{}").expect_err("not a marker line"),
            CommentError::NotWorkGraphEvent
        );
    }

    #[test]
    fn summary_rules_are_enforced() {
        let json = event(all_payloads().remove(0)).to_canonical_json();
        let with_summary = |summary: &str| format!("{COMMENT_MARKER}\n\n{summary}\n\n{json}");

        assert_eq!(
            parse_comment(&with_summary("")).expect_err("empty summary"),
            CommentError::EmptySummary
        );
        assert_eq!(
            parse_comment(&format!("{COMMENT_MARKER}\n\nline one\nline two\n\n{json}"))
                .expect_err("multiline summary"),
            CommentError::MultilineSummary
        );
        let long = "s".repeat(MAX_SUMMARY_CHARS + 1);
        assert_eq!(
            parse_comment(&with_summary(&long)).expect_err("long summary"),
            CommentError::SummaryTooLong(MAX_SUMMARY_CHARS + 1)
        );
        assert_eq!(
            parse_comment(&with_summary(" padded ")).expect_err("padded summary"),
            CommentError::UnnormalizedSummary
        );
        assert!(parse_comment(&with_summary(&"s".repeat(MAX_SUMMARY_CHARS))).is_ok());
    }

    #[test]
    fn summaries_cannot_smuggle_line_breaks_or_bidi_tricks() {
        let json = event(all_payloads().remove(0)).to_canonical_json();
        for smuggled in [
            "line one\u{2028}line two",
            "line one\u{2029}line two",
            "\u{FEFF}leading bom",
            "safe\u{202E}drowssab",
            "safe\u{2066}isolated\u{2069}",
            "safe\u{200E}mark",
        ] {
            let body = format!("{COMMENT_MARKER}\n\n{smuggled}\n\n{json}");
            assert_eq!(
                parse_comment(&body).expect_err("smuggled summary must be rejected"),
                CommentError::UnnormalizedSummary,
                "summary {smuggled:?} was accepted"
            );
            assert_eq!(
                render_comment(&event(all_payloads().remove(0)), smuggled)
                    .expect_err("smuggled summary must not render"),
                CommentError::UnnormalizedSummary
            );
        }
    }

    #[test]
    fn summary_length_counts_characters_not_bytes() {
        let json = event(all_payloads().remove(0)).to_canonical_json();
        let summary = "é".repeat(MAX_SUMMARY_CHARS);
        assert_eq!(summary.len(), MAX_SUMMARY_CHARS * 2);
        let body = format!("{COMMENT_MARKER}\n\n{summary}\n\n{json}");
        assert!(parse_comment(&body).is_ok());
    }

    #[test]
    fn trailing_text_is_rejected() {
        let json = event(all_payloads().remove(0)).to_canonical_json();
        for suffix in ["\n", " ", "\n\nthanks!", "extra"] {
            let body = format!("{COMMENT_MARKER}\n\nSummary line\n\n{json}{suffix}");
            assert_eq!(
                parse_comment(&body).expect_err("trailing text rejected"),
                CommentError::TrailingText,
                "suffix {suffix:?} was accepted"
            );
        }
    }

    #[test]
    fn missing_sections_are_rejected() {
        let json = event(all_payloads().remove(0)).to_canonical_json();
        assert_eq!(
            parse_comment(&format!("{COMMENT_MARKER}\n\n{json}")).expect_err("no summary"),
            CommentError::MissingBlankLineAfterSummary
        );
        assert_eq!(
            parse_comment(&format!("{COMMENT_MARKER}\n\nSummary line\n\n")).expect_err("no json"),
            CommentError::MissingEvent
        );
        assert_eq!(
            parse_comment(COMMENT_MARKER).expect_err("marker only"),
            CommentError::MissingBlankLineAfterMarker
        );
    }

    #[test]
    fn malformed_and_invalid_json_are_distinguished() {
        let body = format!("{COMMENT_MARKER}\n\nSummary line\n\n{{\"a\":");
        assert!(matches!(
            parse_comment(&body).expect_err("malformed json"),
            CommentError::InvalidJson(_)
        ));

        let body = format!("{COMMENT_MARKER}\n\nSummary line\n\n{{\"a\":1}}");
        assert!(matches!(
            parse_comment(&body).expect_err("not an event"),
            CommentError::InvalidEvent(_)
        ));
    }

    #[test]
    fn pretty_printed_json_parses_and_canonicalizes() {
        let event = event(all_payloads().remove(3));
        let value: serde_json::Value =
            serde_json::from_str(&event.to_canonical_json()).expect("value");
        let pretty = serde_json::to_string_pretty(&value).expect("pretty");
        let body = format!("{COMMENT_MARKER}\n\nSummary line\n\n{pretty}");
        let parsed = parse_comment(&body).expect("pretty json parses");
        assert_eq!(parsed.event, event);
        assert_eq!(
            parsed.to_canonical_body(),
            render_comment(&event, "Summary line").expect("render")
        );
    }

    #[test]
    fn render_rejects_invalid_summaries() {
        let event = event(all_payloads().remove(0));
        assert_eq!(
            render_comment(&event, "").expect_err("empty"),
            CommentError::EmptySummary
        );
        assert_eq!(
            render_comment(&event, "a\nb").expect_err("multiline"),
            CommentError::MultilineSummary
        );
        assert_eq!(
            render_comment(&event, &"s".repeat(MAX_SUMMARY_CHARS + 1)).expect_err("too long"),
            CommentError::SummaryTooLong(MAX_SUMMARY_CHARS + 1)
        );
    }
}
