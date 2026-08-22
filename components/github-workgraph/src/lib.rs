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

//! Canonical wire contracts shared by GitHub WorkGraph components.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const TASK_LEASE_MARKER: &str = "WorkGraphTaskLease/v1";
pub const TASK_LEASE_FAMILY: &str = "WorkGraphTaskLease/";
pub const SUPPORTED_AGENT_PROFILES: &[&str] = &["issue-validator", "issue-info-requester"];
pub const MAX_OPAQUE_ID_LEN: usize = 256;

const LEASE_PREFIX: &str = "WorkGraphTaskLease/v1\n\n```json\n";
const FENCE_SUFFIX: &str = "\n```\n";

/// Canonical `WorkGraphTaskLease/v1` wire object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskLease {
    pub lease_id: String,
    pub assignment_comment_node_id: String,
    pub worker_id: String,
    pub slot_id: String,
    pub acquired_at: String,
    pub expires_at: String,
}

/// Validate the typed Lease payload independently of its comment envelope.
pub fn validate_task_lease(lease: &TaskLease) -> Result<(), String> {
    opaque_id(&lease.lease_id, "leaseId")?;
    opaque_id(&lease.assignment_comment_node_id, "assignmentCommentNodeId")?;
    opaque_id(&lease.worker_id, "workerId")?;
    opaque_id(&lease.slot_id, "slotId")?;
    let acquired_at = utc_timestamp(&lease.acquired_at, "acquiredAt")?;
    let expires_at = utc_timestamp(&lease.expires_at, "expiresAt")?;
    if acquired_at >= expires_at {
        return Err("acquiredAt must be strictly earlier than expiresAt".to_string());
    }
    Ok(())
}

/// Build the exact canonical Lease comment body, including one final LF.
pub fn canonical_task_lease_body(lease: &TaskLease) -> Result<String, String> {
    validate_task_lease(lease)?;
    let json = serde_json::to_string_pretty(lease)
        .map_err(|error| format!("failed to serialize task Lease: {error}"))?;
    Ok(format!("{LEASE_PREFIX}{json}{FENCE_SUFFIX}"))
}

/// Parse and validate the exact canonical Lease marker, fence, JSON, and final LF.
pub fn parse_task_lease_body(body: &str) -> Result<TaskLease, String> {
    let json = body
        .strip_prefix(LEASE_PREFIX)
        .and_then(|body| body.strip_suffix(FENCE_SUFFIX))
        .ok_or_else(|| {
            "the WorkGraph task Lease marker, fence, spacing, and final LF must be exact"
                .to_string()
        })?;
    if json.is_empty() || json.contains("\n```") {
        return Err(
            "the WorkGraph task Lease must contain exactly one fenced JSON object".to_string(),
        );
    }
    let lease: TaskLease =
        serde_json::from_str(json).map_err(|error| format!("invalid task Lease JSON: {error}"))?;
    validate_task_lease(&lease)?;
    let canonical = serde_json::to_string_pretty(&lease)
        .map_err(|error| format!("failed to serialize task Lease: {error}"))?;
    if canonical != json {
        return Err(
            "the WorkGraph task Lease JSON must use canonical two-space typed formatting"
                .to_string(),
        );
    }
    Ok(lease)
}

/// Extract a Lease ID from a Lease-family comment when its JSON is parseable.
///
/// Reconciliation uses this only to fail closed on a non-canonical comment that
/// claims an ID already reserved by the dispatcher.
pub fn candidate_task_lease_id(body: &str) -> Option<String> {
    if let Ok(lease) = parse_task_lease_body(body) {
        return Some(lease.lease_id);
    }
    if !body.starts_with(TASK_LEASE_FAMILY) {
        return None;
    }
    let fenced = body.split_once("```json")?.1;
    let json = fenced.split("```").next()?.trim();
    serde_json::from_str::<serde_json::Value>(json)
        .ok()?
        .get("leaseId")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn opaque_id(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_OPAQUE_ID_LEN
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(format!(
            "{field} must be a non-empty identifier of at most {MAX_OPAQUE_ID_LEN} characters \
             with no whitespace or control characters"
        ));
    }
    Ok(())
}

fn utc_timestamp(value: &str, field: &str) -> Result<DateTime<Utc>, String> {
    let invalid = || {
        format!(
            "{field} must be a canonical UTC timestamp of the exact form \
             'YYYY-MM-DDTHH:MM:SSZ', for example '2026-08-19T22:00:00Z'"
        )
    };
    let bytes = value.as_bytes();
    let shape_ok = bytes.len() == 20
        && value.ends_with('Z')
        && bytes[10] == b'T'
        && [4, 7].iter().all(|index| bytes[*index] == b'-')
        && [13, 16].iter().all(|index| bytes[*index] == b':')
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .iter()
            .all(|index| bytes[*index].is_ascii_digit());
    if !shape_ok {
        return Err(invalid());
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> TaskLease {
        TaskLease {
            lease_id: "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21".to_string(),
            assignment_comment_node_id: "IC_assignment".to_string(),
            worker_id: "validator-1".to_string(),
            slot_id: "validator-1/1".to_string(),
            acquired_at: "2026-08-19T22:00:00Z".to_string(),
            expires_at: "2026-08-19T22:15:00Z".to_string(),
        }
    }

    #[test]
    fn canonical_body_round_trips_byte_exactly() {
        let lease = lease();
        let body = canonical_task_lease_body(&lease).unwrap();
        assert_eq!(
            body,
            "WorkGraphTaskLease/v1\n\n```json\n{\n  \"leaseId\": \
             \"0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21\",\n  \
             \"assignmentCommentNodeId\": \"IC_assignment\",\n  \"workerId\": \
             \"validator-1\",\n  \"slotId\": \"validator-1/1\",\n  \"acquiredAt\": \
             \"2026-08-19T22:00:00Z\",\n  \"expiresAt\": \
             \"2026-08-19T22:15:00Z\"\n}\n```\n"
        );
        assert_eq!(parse_task_lease_body(&body).unwrap(), lease);
    }

    #[test]
    fn parser_rejects_noncanonical_json_and_timestamps() {
        let compact = "WorkGraphTaskLease/v1\n\n```json\n{\"leaseId\":\"x\"}\n```\n";
        assert!(parse_task_lease_body(compact).is_err());
        let mut lease = lease();
        lease.acquired_at = "2026-08-19T22:00:00.000Z".to_string();
        assert!(canonical_task_lease_body(&lease).is_err());
    }

    #[test]
    fn candidate_id_fails_closed_for_parseable_noncanonical_body() {
        let body = "WorkGraphTaskLease/v1\n\n```json\n{\"leaseId\":\"claim\"}\n```\n";
        assert_eq!(candidate_task_lease_id(body).as_deref(), Some("claim"));
        let body = "WorkGraphTaskLease/v2\n ```json\n{ \"leaseId\": \"claim-v2\" }\n```";
        assert_eq!(candidate_task_lease_id(body).as_deref(), Some("claim-v2"));
    }
}
