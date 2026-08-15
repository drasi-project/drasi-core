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

use serde::{Deserialize, Serialize};

/// Minimal locator parsed from a webhook payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebhookLocator {
    pub event_type: String,
    pub action: String,
    pub node_id: Option<String>,
    pub repository_full_name: Option<String>,
    pub parent_issue_id: Option<String>,
    pub parent_pull_request_id: Option<String>,
    pub project_id: Option<String>,
    pub project_owner: Option<String>,
    pub project_number: Option<u32>,
}

impl WebhookLocator {
    pub fn deleted_node_label(&self) -> Option<&'static str> {
        match self.event_type.as_str() {
            "projects_v2" => Some("GitHubProject"),
            "projects_v2_item" => Some("GitHubProjectItem"),
            "repository" => Some("GitHubRepository"),
            "issues" => Some("GitHubIssue"),
            "pull_request" => Some("GitHubPullRequest"),
            "issue_comment" => Some("GitHubIssueComment"),
            "pull_request_review" => Some("GitHubPullRequestReview"),
            "pull_request_review_comment" => Some("GitHubPullRequestReviewComment"),
            _ => None,
        }
    }
}

/// Hydrator degradation state used by `/health`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HydratorHealth {
    pub stalled_delivery_id: Option<String>,
    pub retry_count: u32,
    pub next_retry_secs: Option<u64>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub terminal: bool,
}
