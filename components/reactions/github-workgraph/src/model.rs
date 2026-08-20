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

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CapacityRow {
    pub repository_owner: String,
    pub repository_name: String,
    pub worker_id: String,
    pub lease_duration_seconds: i64,
    pub active_lease_ids: Vec<String>,
    pub free_slot_ids: Vec<String>,
    pub dispatchable_tasks: Vec<DispatchableTask>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DispatchableTask {
    pub task_node_id: String,
    pub task_number: u64,
    pub repository_owner: String,
    pub repository_name: String,
    pub assignment_comment_node_id: String,
    pub worker_id: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PendingScope {
    pub repository_owner: String,
    pub repository_name: String,
    pub worker_id: String,
}

impl From<&CapacityRow> for PendingScope {
    fn from(row: &CapacityRow) -> Self {
        Self {
            repository_owner: row.repository_owner.clone(),
            repository_name: row.repository_name.clone(),
            worker_id: row.worker_id.clone(),
        }
    }
}

pub(crate) struct PendingLease {
    pub scope: PendingScope,
    pub slot_id: String,
    pub task_node_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskLease {
    lease_id: String,
    assignment_comment_node_id: String,
    worker_id: String,
    slot_id: String,
    acquired_at: String,
    expires_at: String,
}

#[derive(Serialize)]
pub(crate) struct IssueComment {
    pub body: String,
}

pub(crate) fn lease_comment(
    lease_id: &str,
    slot_id: &str,
    task: &DispatchableTask,
    acquired_at: DateTime<Utc>,
    lease_duration_seconds: i64,
) -> Result<String> {
    let expires_at = acquired_at + chrono::Duration::seconds(lease_duration_seconds);
    let lease = TaskLease {
        lease_id: lease_id.to_string(),
        assignment_comment_node_id: task.assignment_comment_node_id.clone(),
        worker_id: task.worker_id.clone(),
        slot_id: slot_id.to_string(),
        acquired_at: acquired_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        expires_at: expires_at.to_rfc3339_opts(SecondsFormat::Secs, true),
    };
    let json = serde_json::to_string_pretty(&lease)?;
    Ok(format!("WorkGraphTaskLease/v1\n\n```json\n{json}\n```\n"))
}
