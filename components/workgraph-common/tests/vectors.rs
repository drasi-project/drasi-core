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

//! Cross-language test vectors for `workgraph.event/v1`.
//!
//! `vectors/workgraph-event-v1.vectors.json` is generated from this file and is
//! the contract non-Rust consumers implement against — in particular the
//! JavaScript `workgraph/report_completion` reporter, which must derive the same
//! `runId`/`eventId` and render the same comment body as the Rust reactions.
//!
//! ```sh
//! cargo test -p drasi-workgraph-common --test vectors            # verify
//! DRASI_UPDATE_VECTORS=1 cargo test -p drasi-workgraph-common --test vectors  # regenerate
//! ```

use std::path::PathBuf;

use drasi_workgraph_common::{
    comment::{parse_comment, render_comment},
    event::{
        AssignedResponsibilityType, CompletedIssueValidationPayload, ExecutionId,
        ExecutionStartedPayload, ProfileRef, ResponsibilityAssignedPayload, RoutingDecidedPayload,
        ValidationOutcome, ValidationReasonCode, WorkGraphEvent, WorkGraphEventPayload,
        WorkGraphEventType,
    },
    ids::{body_digest, event_id, run_id},
    summary::summary_for,
};
use serde_json::{json, Value};

const ITEM: &str = "PVTI_example";
const SUBJECT: &str = "I_example";
const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";
const REPOSITORY: &str = "drasi-project/drasi-core";
const NUMBER: u64 = 742;
const BODY: &str = "Context\nWorkGraph-Validation: pass\n";

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vectors")
        .join("workgraph-event-v1.vectors.json")
}

fn sample_events() -> Vec<(&'static str, WorkGraphEvent)> {
    let digest = body_digest(Some(BODY));
    let run = run_id(ITEM, &digest);
    let execution = ExecutionId::from_run_id(&run);

    vec![
        (
            "responsibility-assigned",
            WorkGraphEvent::new(
                run.clone(),
                ITEM,
                SUBJECT,
                WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
                    responsibility_type: AssignedResponsibilityType::IssueValidation,
                    profile_ref: ProfileRef::new("issue-validator", BLOB).expect("valid profile"),
                    content_digest: digest.clone(),
                }),
            )
            .expect("valid event"),
        ),
        (
            "execution-started",
            WorkGraphEvent::new(
                run.clone(),
                ITEM,
                SUBJECT,
                WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                    execution_id: execution.clone(),
                    task_id: "agent-task-1234".to_string(),
                }),
            )
            .expect("valid event"),
        ),
        (
            "completed-issue-validation-passed",
            WorkGraphEvent::new(
                run.clone(),
                ITEM,
                SUBJECT,
                WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                    execution_id: execution.clone(),
                    outcome: ValidationOutcome::Passed,
                    reason_code: ValidationReasonCode::RequiredMarkerPresent,
                }),
            )
            .expect("valid event"),
        ),
        (
            "routing-decided-passed",
            WorkGraphEvent::new(
                run.clone(),
                ITEM,
                SUBJECT,
                WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
                    ValidationOutcome::Passed,
                )),
            )
            .expect("valid event"),
        ),
        (
            "completed-issue-validation-failed",
            WorkGraphEvent::new(
                run.clone(),
                ITEM,
                SUBJECT,
                WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                    execution_id: execution,
                    outcome: ValidationOutcome::Failed,
                    reason_code: ValidationReasonCode::RequiredMarkerMissing,
                }),
            )
            .expect("valid event"),
        ),
        (
            "routing-decided-failed",
            WorkGraphEvent::new(
                run,
                ITEM,
                SUBJECT,
                WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
                    ValidationOutcome::Failed,
                )),
            )
            .expect("valid event"),
        ),
    ]
}

fn invalid_comment_cases() -> Vec<(&'static str, String, &'static str)> {
    let json = sample_events()[0].1.to_canonical_json();
    vec![
        (
            "legacy-pure-json",
            json.clone(),
            "NotWorkGraphEvent: legacy pure-JSON comments are ignored, not migrated",
        ),
        (
            "legacy-fenced",
            format!("WorkGraphEvent/v1\n```json\n{json}\n```"),
            "MissingBlankLineAfterMarker: fenced bodies are rejected, not migrated",
        ),
        (
            "empty-summary",
            format!("WorkGraphEvent/v1\n\n\n\n{json}"),
            "EmptySummary",
        ),
        (
            "multiline-summary",
            format!("WorkGraphEvent/v1\n\nline one\nline two\n\n{json}"),
            "MultilineSummary",
        ),
        (
            "summary-too-long",
            format!("WorkGraphEvent/v1\n\n{}\n\n{json}", "s".repeat(121)),
            "SummaryTooLong: summaries are capped at 120 characters",
        ),
        (
            "trailing-text",
            format!("WorkGraphEvent/v1\n\nSummary line\n\n{json}\n\nthanks!"),
            "TrailingText: the JSON object must end the comment",
        ),
        (
            "trailing-newline",
            format!("WorkGraphEvent/v1\n\nSummary line\n\n{json}\n"),
            "TrailingText: not even trailing whitespace is allowed",
        ),
        (
            "line-separator-summary",
            format!("WorkGraphEvent/v1\n\nline one\u{2028}line two\n\n{json}"),
            "UnnormalizedSummary: U+2028/U+2029 would render as a second summary line",
        ),
        (
            "bidi-override-summary",
            format!("WorkGraphEvent/v1\n\nsafe\u{202E}drowssab\n\n{json}"),
            "UnnormalizedSummary: bidirectional overrides can misrepresent the summary to a reader",
        ),
        (
            "unknown-envelope-field",
            format!(
                "WorkGraphEvent/v1\n\nSummary line\n\n{}",
                json.replacen('{', r#"{"actor":"mallory","#, 1)
            ),
            "InvalidEvent: the envelope carries no actor; identity comes from GitHub metadata",
        ),
        (
            "unknown-payload-field",
            format!(
                "WorkGraphEvent/v1\n\nSummary line\n\n{}",
                json.replacen(
                    r#""payload":{"#,
                    r#""payload":{"timestamp":"2026-01-01T00:00:00Z","#,
                    1
                )
            ),
            "InvalidEvent: payloads deny unknown fields",
        ),
    ]
}

fn build_vectors() -> Value {
    let digest = body_digest(Some(BODY));
    let run = run_id(ITEM, &digest);

    let body_digests: Vec<Value> = [
        ("null-body", None),
        ("empty-body", Some("")),
        ("sample-body", Some(BODY)),
        ("edited-body", Some("Context\n")),
    ]
    .into_iter()
    .map(|(name, body)| {
        json!({
            "name": name,
            "body": body,
            "bodyDigest": body_digest(body).as_str(),
        })
    })
    .collect();

    let run_ids: Vec<Value> = [
        ("sample", ITEM, BODY),
        ("other-project-item", "PVTI_other", BODY),
        ("edited-body", ITEM, "Context\n"),
    ]
    .into_iter()
    .map(|(name, item, body)| {
        let digest = body_digest(Some(body));
        json!({
            "name": name,
            "projectItemNodeId": item,
            "bodyDigest": digest.as_str(),
            "runId": run_id(item, &digest).as_str(),
        })
    })
    .collect();

    let event_ids: Vec<Value> = WorkGraphEventType::ALL
        .iter()
        .map(|event_type| {
            json!({
                "runId": run.as_str(),
                "eventType": event_type.as_str(),
                "eventId": event_id(&run, *event_type).as_str(),
            })
        })
        .collect();

    let comments: Vec<Value> = sample_events()
        .into_iter()
        .map(|(name, event)| {
            let summary = summary_for(&event);
            let body = render_comment(&event, &summary).expect("render");
            json!({
                "name": name,
                "summary": summary,
                "canonicalJson": event.to_canonical_json(),
                "commentBody": body,
            })
        })
        .collect();

    let invalid: Vec<Value> = invalid_comment_cases()
        .into_iter()
        .map(|(name, body, reason)| {
            json!({
                "name": name,
                "commentBody": body,
                "rejectedBecause": reason,
            })
        })
        .collect();

    json!({
        "schemaVersion": drasi_workgraph_common::SCHEMA_VERSION,
        "note": "Generated by `cargo test -p drasi-workgraph-common --test vectors`. Do not edit by hand.",
        "algorithms": {
            "bodyDigest": "\"sha256:\" + lowerHex(sha256(utf8(body ?? \"\")))",
            "runId": "\"validation:\" + projectItemNodeId + \":\" + bodyDigest",
            "eventId": "\"event:\" + runId + \":\" + eventType",
            "executionId": "\"execution:\" + runId",
            "notes": [
                "projectItemNodeId must match exactly PVTI_[A-Za-z0-9]+.",
                "bodyDigest must match exactly sha256:[0-9a-f]{64}.",
                "subjectNodeId is not an input to runId.",
                "eventType is the exact token, e.g. 'CompletedIssueValidation'.",
                "Comment grammar: 'WorkGraphEvent/v1' LF LF summary LF LF json, no fence, no trailing text.",
                "Parsers must normalize CRLF to LF before applying the grammar.",
                "Summaries are generated for humans and must never be parsed for meaning."
            ]
        },
        "commentMarker": drasi_workgraph_common::COMMENT_MARKER,
        "maxSummaryChars": drasi_workgraph_common::MAX_SUMMARY_CHARS,
        "subject": {
            "repository": REPOSITORY,
            "number": NUMBER,
            "projectItemNodeId": ITEM,
            "subjectNodeId": SUBJECT
        },
        "bodyDigests": body_digests,
        "runIds": run_ids,
        "eventIds": event_ids,
        "comments": comments,
        "invalidComments": invalid
    })
}

#[test]
fn vectors_file_is_in_sync() {
    let generated = build_vectors();
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&generated).expect("vectors serialize")
    );
    let path = vectors_path();

    if std::env::var("DRASI_UPDATE_VECTORS").is_ok() {
        std::fs::write(&path, &rendered).expect("write vectors");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "missing {}: {error}. Regenerate with DRASI_UPDATE_VECTORS=1.",
            path.display()
        )
    });
    assert_eq!(
        committed,
        rendered,
        "{} is stale; regenerate with DRASI_UPDATE_VECTORS=1",
        path.display()
    );
}

#[test]
fn committed_vectors_replay_against_the_implementation() {
    let committed: Value =
        serde_json::from_str(&std::fs::read_to_string(vectors_path()).expect("read vectors"))
            .expect("vectors are valid JSON");

    for case in committed["bodyDigests"].as_array().expect("bodyDigests") {
        let body = case["body"].as_str();
        assert_eq!(
            body_digest(body).as_str(),
            case["bodyDigest"].as_str().expect("digest"),
            "body digest vector '{}' drifted",
            case["name"]
        );
    }

    for case in committed["runIds"].as_array().expect("runIds") {
        let digest = drasi_workgraph_common::Sha256Digest::try_from(
            case["bodyDigest"].as_str().expect("digest").to_string(),
        )
        .expect("valid digest");
        assert_eq!(
            run_id(case["projectItemNodeId"].as_str().expect("item"), &digest).as_str(),
            case["runId"].as_str().expect("runId"),
            "run id vector '{}' drifted",
            case["name"]
        );
    }

    for case in committed["eventIds"].as_array().expect("eventIds") {
        let run = drasi_workgraph_common::RunId::try_from(
            case["runId"].as_str().expect("runId").to_string(),
        )
        .expect("valid run id");
        let event_type = WorkGraphEventType::ALL
            .iter()
            .find(|candidate| candidate.as_str() == case["eventType"].as_str().expect("type"))
            .copied()
            .expect("known event type");
        assert_eq!(
            event_id(&run, event_type).as_str(),
            case["eventId"].as_str().expect("eventId"),
            "event id vector for '{event_type}' drifted"
        );
    }

    for case in committed["comments"].as_array().expect("comments") {
        let body = case["commentBody"].as_str().expect("body");
        let parsed = parse_comment(body).expect("committed comment vector must parse");
        assert_eq!(
            parsed.event.to_canonical_json(),
            case["canonicalJson"].as_str().expect("canonical json"),
            "comment vector '{}' drifted",
            case["name"]
        );
        assert_eq!(parsed.summary, case["summary"].as_str().expect("summary"));
        assert_eq!(parsed.to_canonical_body(), body);
    }

    for case in committed["invalidComments"].as_array().expect("invalid") {
        let body = case["commentBody"].as_str().expect("body");
        assert!(
            parse_comment(body).is_err(),
            "invalid vector '{}' unexpectedly parsed",
            case["name"]
        );
    }
}

#[test]
fn dogfood_completion_fixture_matches_rust_byte_for_byte() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/completed-validation-passed.json"))
            .expect("dogfood fixture is valid JSON");
    let expected = fixture["comment"]["body"]
        .as_str()
        .expect("fixture comment body");
    let event = sample_events()
        .into_iter()
        .find(|(name, _)| *name == "completed-issue-validation-passed")
        .map(|(_, event)| event)
        .expect("completion event");
    let rendered = render_comment(&event, &summary_for(&event)).expect("render event");

    assert_eq!(rendered.as_bytes(), expected.as_bytes());
    assert!(!rendered.ends_with('\n'));
    assert_eq!(
        parse_comment(expected).expect("fixture parses").event,
        event
    );
}
