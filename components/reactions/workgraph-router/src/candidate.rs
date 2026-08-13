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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutingCandidate {
    pub execution_id: String,
    pub required_event_type: String,
    pub event_id: String,
    pub event_type: String,
    pub outcome: String,
    pub subject_repo: String,
    pub subject_issue_number: u64,
    pub project_id: String,
    pub project_item_id: String,
    pub project_status: String,
    pub route_id: String,
    pub route_expected_event_id: String,
    pub route_expected_event_type: String,
    pub route_expected_subject_repo: String,
    pub route_expected_subject_issue_number: u64,
    pub route_content_version: String,
    pub route_content_profile: String,
    pub responsibility_id: String,
    pub responsibility_type: String,
    pub responsibility_actor: String,
    pub submitter_actor: String,
    pub launcher_author: String,
    pub agent_author: String,
    pub router_author: String,
    pub routing_author: String,
    #[serde(default)]
    pub observed_authors: Vec<String>,
    pub comment_id: u64,
    pub comment_author: String,
    pub comment_body: String,
    pub comment_edited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_updated_at: Option<String>,
    pub comment_provenance_event_id: String,
    pub comment_provenance_event_type: String,
    pub content_version: String,
    pub content_profile: String,
}

impl RoutingCandidate {
    pub fn reservation_key(&self) -> String {
        format!("{}:{}", self.execution_id, self.required_event_type)
    }

    pub fn outcome_key(&self) -> &str {
        self.outcome.trim()
    }
}
