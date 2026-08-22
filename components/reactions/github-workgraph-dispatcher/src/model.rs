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

use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use drasi_github_workgraph::{canonical_task_lease_body, TaskLease, MAX_OPAQUE_ID_LEN};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const RESERVATION_PREFIX: &str = "github-workgraph-dispatcher/reservation/";
pub const CURSOR_PREFIX: &str = "github-workgraph-dispatcher/cursor/";
pub const IDENTITY_KEY: &str = "github-workgraph-dispatcher/identity";
pub const BOOTSTRAP_WATERMARK_KEY: &str = "github-workgraph-dispatcher/bootstrap-watermark";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DispatchableTask {
    pub task_node_id: String,
    #[serde(deserialize_with = "deserialize_u64_integer")]
    pub task_number: u64,
    pub repository_owner: String,
    pub repository_name: String,
    pub assignment_comment_node_id: String,
    pub worker_id: String,
    pub task_type: String,
    #[serde(deserialize_with = "deserialize_i64_integer")]
    pub queue_priority: i64,
    pub assignment_created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapacityRow {
    pub repository_owner: String,
    pub repository_name: String,
    pub worker_id: String,
    pub agent_profile: String,
    #[serde(deserialize_with = "deserialize_i64_integer")]
    pub lease_duration_seconds: i64,
    #[serde(deserialize_with = "deserialize_u32_integer")]
    pub configured_slot_count: u32,
    #[serde(deserialize_with = "deserialize_u32_integer")]
    pub active_lease_count: u32,
    pub active_lease_ids: Vec<String>,
    pub free_slot_ids: Vec<String>,
    pub dispatchable_task_ids: Vec<String>,
    pub dispatchable_tasks: Vec<DispatchableTask>,
}

impl CapacityRow {
    pub fn validate(&self) -> Result<()> {
        github_name(&self.repository_owner, "repositoryOwner")?;
        github_name(&self.repository_name, "repositoryName")?;
        worker_id(&self.worker_id, "workerId")?;
        ensure!(
            drasi_github_workgraph::SUPPORTED_AGENT_PROFILES.contains(&self.agent_profile.as_str()),
            "agentProfile must be one of: {}",
            drasi_github_workgraph::SUPPORTED_AGENT_PROFILES.join(", ")
        );
        ensure!(
            (1..=86_400).contains(&self.lease_duration_seconds),
            "leaseDurationSeconds must be between 1 and 86400"
        );
        ensure!(
            (1..=16).contains(&self.configured_slot_count),
            "configuredSlotCount must be between 1 and 16"
        );
        ensure!(
            self.active_lease_count as usize == self.active_lease_ids.len(),
            "activeLeaseCount must equal the number of activeLeaseIds"
        );
        ensure!(
            self.free_slot_ids.len() <= self.configured_slot_count as usize,
            "freeSlotIds cannot contain more entries than configuredSlotCount"
        );
        unique_ids(&self.active_lease_ids, "activeLeaseIds")?;
        unique_ids(&self.free_slot_ids, "freeSlotIds")?;
        unique_ids(&self.dispatchable_task_ids, "dispatchableTaskIds")?;

        let slot_prefix = format!("{}/", self.worker_id);
        for (index, slot_id) in self.free_slot_ids.iter().enumerate() {
            opaque_id(slot_id, &format!("freeSlotIds[{index}]"))?;
            let number = slot_id
                .strip_prefix(&slot_prefix)
                .and_then(|value| value.parse::<u32>().ok())
                .with_context(|| {
                    format!(
                        "freeSlotIds[{index}] must be a deterministic slot ID for worker '{}'",
                        self.worker_id
                    )
                })?;
            ensure!(
                (1..=self.configured_slot_count).contains(&number),
                "freeSlotIds[{index}] names slot {number} outside configured capacity {}",
                self.configured_slot_count
            );
        }

        ensure!(
            self.dispatchable_task_ids.len() == self.dispatchable_tasks.len(),
            "dispatchableTaskIds and dispatchableTasks must have the same length"
        );
        let mut task_numbers = HashSet::with_capacity(self.dispatchable_tasks.len());
        let mut assignment_ids = HashSet::with_capacity(self.dispatchable_tasks.len());
        for (index, task) in self.dispatchable_tasks.iter().enumerate() {
            task.validate(index, self)?;
            ensure!(
                self.dispatchable_task_ids[index] == task.task_node_id,
                "dispatchableTaskIds[{index}] must equal dispatchableTasks[{index}].taskNodeId"
            );
            ensure!(
                task_numbers.insert(task.task_number),
                "dispatchableTasks[{index}].taskNumber duplicates task {}",
                task.task_number
            );
            ensure!(
                assignment_ids.insert(&task.assignment_comment_node_id),
                "dispatchableTasks[{index}].assignmentCommentNodeId duplicates '{}'",
                task.assignment_comment_node_id
            );
        }
        Ok(())
    }
}

impl DispatchableTask {
    fn validate(&self, index: usize, row: &CapacityRow) -> Result<()> {
        let field = |name: &str| format!("dispatchableTasks[{index}].{name}");
        opaque_id(&self.task_node_id, &field("taskNodeId"))?;
        ensure!(
            self.task_number > 0,
            "{} must be positive",
            field("taskNumber")
        );
        github_name(&self.repository_owner, &field("repositoryOwner"))?;
        github_name(&self.repository_name, &field("repositoryName"))?;
        opaque_id(
            &self.assignment_comment_node_id,
            &field("assignmentCommentNodeId"),
        )?;
        worker_id(&self.worker_id, &field("workerId"))?;
        ensure!(
            self.repository_owner == row.repository_owner
                && self.repository_name == row.repository_name,
            "dispatchableTasks[{index}] repository must match the capacity row"
        );
        ensure!(
            self.worker_id == row.worker_id,
            "dispatchableTasks[{index}].workerId must match the capacity row"
        );
        ensure!(
            matches!(self.task_type.as_str(), "validate-issue" | "request-info"),
            "{} must be 'validate-issue' or 'request-info'",
            field("taskType")
        );
        ensure!(
            matches!(
                (row.agent_profile.as_str(), self.task_type.as_str()),
                ("issue-validator", "validate-issue")
                    | ("issue-info-requester", "request-info")
            ),
            "dispatchableTasks[{index}].taskType is incompatible with the capacity row agentProfile"
        );
        canonical_utc(&self.assignment_created_at, &field("assignmentCreatedAt"))?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationPhase {
    Reserved,
    WriteInFlight,
    AwaitingProjection,
    Confirmed,
    ReconcileRequired,
}

impl ReservationPhase {
    pub fn overlays_capacity(self) -> bool {
        !matches!(self, Self::Confirmed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reservation {
    pub schema_version: u32,
    pub lease_id: String,
    pub query_id: String,
    pub worker_id: String,
    pub agent_profile: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub task_node_id: String,
    pub task_number: u64,
    pub assignment_comment_node_id: String,
    pub slot_id: String,
    pub task_type: String,
    pub acquired_at: String,
    pub expires_at: String,
    pub canonical_body: String,
    pub body_digest: String,
    pub phase: ReservationPhase,
    pub attempt_count: u32,
    pub last_error: Option<String>,
    pub origin_sequence: u64,
    pub origin_row_signature: u64,
    pub lease_comment_node_id: Option<String>,
    pub lease_comment_database_id: Option<u64>,
}

impl Reservation {
    pub fn key(&self) -> String {
        format!("{RESERVATION_PREFIX}{}", self.lease_id)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported reservation schemaVersion"
        );
        opaque_id(&self.lease_id, "reservation.leaseId")?;
        ensure!(
            !self.query_id.trim().is_empty(),
            "reservation.queryId must not be empty"
        );
        worker_id(&self.worker_id, "reservation.workerId")?;
        ensure!(
            drasi_github_workgraph::SUPPORTED_AGENT_PROFILES.contains(&self.agent_profile.as_str()),
            "reservation.agentProfile is unsupported"
        );
        github_name(&self.repository_owner, "reservation.repositoryOwner")?;
        github_name(&self.repository_name, "reservation.repositoryName")?;
        opaque_id(&self.task_node_id, "reservation.taskNodeId")?;
        ensure!(
            self.task_number > 0,
            "reservation.taskNumber must be positive"
        );
        opaque_id(
            &self.assignment_comment_node_id,
            "reservation.assignmentCommentNodeId",
        )?;
        opaque_id(&self.slot_id, "reservation.slotId")?;
        ensure!(
            matches!(self.task_type.as_str(), "validate-issue" | "request-info"),
            "reservation.taskType is unsupported"
        );
        let lease = self.task_lease();
        let expected_body = canonical_task_lease_body(&lease)
            .map_err(anyhow::Error::msg)
            .context("reservation contains an invalid task Lease")?;
        ensure!(
            expected_body == self.canonical_body,
            "reservation canonicalBody does not match its Lease fields"
        );
        ensure!(
            sha256_digest(&self.canonical_body) == self.body_digest,
            "reservation bodyDigest does not match canonicalBody"
        );
        if let Some(node_id) = &self.lease_comment_node_id {
            opaque_id(node_id, "reservation.leaseCommentNodeId")?;
        }
        if let Some(database_id) = self.lease_comment_database_id {
            ensure!(
                database_id > 0,
                "reservation.leaseCommentDatabaseId must be positive"
            );
        }
        Ok(())
    }

    pub fn task_lease(&self) -> TaskLease {
        TaskLease {
            lease_id: self.lease_id.clone(),
            assignment_comment_node_id: self.assignment_comment_node_id.clone(),
            worker_id: self.worker_id.clone(),
            slot_id: self.slot_id.clone(),
            acquired_at: self.acquired_at.clone(),
            expires_at: self.expires_at.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerCursor {
    pub schema_version: u32,
    pub query_id: String,
    pub worker_id: String,
    pub sequence: u64,
    pub row_signature: u64,
}

impl WorkerCursor {
    pub fn key(query_id: &str, worker_id: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(query_id.as_bytes());
        hasher.update([0]);
        hasher.update(worker_id.as_bytes());
        format!("{CURSOR_PREFIX}{}", hex::encode(hasher.finalize()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DispatcherIdentity {
    pub schema_version: u32,
    pub query_id: String,
    pub api_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapWatermark {
    pub schema_version: u32,
    pub query_id: String,
    pub sequence: u64,
}

pub fn sha256_digest(value: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(value.as_bytes())))
}

pub fn canonical_utc(value: &str, field: &str) -> Result<DateTime<Utc>> {
    let bytes = value.as_bytes();
    let shape_ok = bytes.len() == 20
        && value.ends_with('Z')
        && bytes[10] == b'T'
        && [4, 7].iter().all(|index| bytes[*index] == b'-')
        && [13, 16].iter().all(|index| bytes[*index] == b':')
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .iter()
            .all(|index| bytes[*index].is_ascii_digit());
    ensure!(
        shape_ok,
        "{field} must use canonical second-precision UTC RFC 3339"
    );
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .with_context(|| format!("{field} is not a valid timestamp"))
}

fn opaque_id(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= MAX_OPAQUE_ID_LEN
            && !value.chars().any(char::is_whitespace)
            && !value.chars().any(char::is_control),
        "{field} must be a non-empty identifier of at most {MAX_OPAQUE_ID_LEN} characters without \
         whitespace or control characters"
    );
    Ok(())
}

fn worker_id(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte)),
        "{field} must contain 1 to 64 ASCII letters, digits, '-', '.', or '_'"
    );
    Ok(())
}

fn github_name(value: &str, field: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && !matches!(value, "." | "..")
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte)),
        "{field} must contain 1 to 100 ASCII letters, digits, '-', '.', or '_'"
    );
    Ok(())
}

fn unique_ids(values: &[String], field: &str) -> Result<()> {
    let mut seen = HashSet::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        opaque_id(value, &format!("{field}[{index}]"))?;
        ensure!(
            seen.insert(value),
            "{field}[{index}] duplicates identifier '{value}'"
        );
    }
    Ok(())
}

fn deserialize_u32_integer<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_u64_integer(deserializer).and_then(|value| {
        u32::try_from(value).map_err(|_| serde::de::Error::custom("integer exceeds u32"))
    })
}

fn deserialize_u64_integer<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if let Some(value) = value.as_u64() {
        return Ok(value);
    }
    if let Some(value) = value.as_f64() {
        if value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64 {
            return Ok(value as u64);
        }
    }
    Err(serde::de::Error::custom(
        "expected a non-negative integer-valued number",
    ))
}

fn deserialize_i64_integer<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if let Some(value) = value.as_i64() {
        return Ok(value);
    }
    if let Some(value) = value.as_f64() {
        if value.is_finite()
            && value.fract() == 0.0
            && value >= i64::MIN as f64
            && value <= i64::MAX as f64
        {
            return Ok(value as i64);
        }
    }
    Err(serde::de::Error::custom(
        "expected an integer-valued number",
    ))
}
