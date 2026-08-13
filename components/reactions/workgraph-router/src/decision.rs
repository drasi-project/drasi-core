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

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::candidate::RoutingCandidate;
use crate::config::WorkgraphRouterReactionConfig;
use crate::rules::PolicyOutcome;

const DECISION_NAMESPACE: Uuid = Uuid::from_bytes([
    0x7e, 0x57, 0x77, 0x3d, 0xd0, 0xef, 0x4f, 0x03, 0x8c, 0x84, 0x0e, 0x56, 0x0d, 0x71, 0x33, 0x1d,
]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingDecision {
    pub decision_id: String,
    pub policy_id: String,
    pub policy_type: String,
    pub policy_version: String,
    pub from_status: String,
    pub to_status: String,
    pub next_responsibility_type: String,
    pub next_responsibility_owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_request: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionCommentEnvelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub decision_id: String,
    pub policy: PolicyIdentity,
    pub source_event: SourceEventProvenance,
    pub subject: SubjectReference,
    pub transition: DecisionTransition,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponsibilityCommentEnvelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub decision_id: String,
    pub responsibility_type: String,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_request: Option<String>,
    pub route_id: String,
    pub responsibility_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyIdentity {
    pub id: String,
    pub policy_type: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceEventProvenance {
    pub execution_id: String,
    pub required_event_type: String,
    pub event_id: String,
    pub event_type: String,
    pub comment_id: u64,
    pub content_version: String,
    pub content_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubjectReference {
    pub repo: String,
    pub issue_number: u64,
    pub project_id: String,
    pub project_item_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionTransition {
    pub from_status: String,
    pub to_status: String,
    pub next_responsibility_type: String,
}

impl RoutingDecision {
    pub fn from_policy(
        config: &WorkgraphRouterReactionConfig,
        candidate: &RoutingCandidate,
        policy: PolicyOutcome,
    ) -> Self {
        let decision_id = deterministic_decision_id(config, candidate, &policy).to_string();
        Self {
            decision_id,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            from_status: policy.from_status,
            to_status: policy.to_status,
            next_responsibility_type: policy.next_responsibility_type,
            next_responsibility_owner: policy.next_responsibility_owner,
            marker_request: policy.marker_request,
            reason: policy.reason,
        }
    }

    pub fn decision_comment(&self, candidate: &RoutingCandidate) -> anyhow::Result<String> {
        let payload = DecisionCommentEnvelope {
            kind: "workgraph.routing-decision/v1".to_string(),
            decision_id: self.decision_id.clone(),
            policy: PolicyIdentity {
                id: self.policy_id.clone(),
                policy_type: self.policy_type.clone(),
                version: self.policy_version.clone(),
            },
            source_event: SourceEventProvenance {
                execution_id: candidate.execution_id.clone(),
                required_event_type: candidate.required_event_type.clone(),
                event_id: candidate.event_id.clone(),
                event_type: candidate.event_type.clone(),
                comment_id: candidate.comment_id,
                content_version: candidate.content_version.clone(),
                content_profile: candidate.content_profile.clone(),
            },
            subject: SubjectReference {
                repo: candidate.subject_repo.clone(),
                issue_number: candidate.subject_issue_number,
                project_id: candidate.project_id.clone(),
                project_item_id: candidate.project_item_id.clone(),
            },
            transition: DecisionTransition {
                from_status: self.from_status.clone(),
                to_status: self.to_status.clone(),
                next_responsibility_type: self.next_responsibility_type.clone(),
            },
            created_at: Utc::now().to_rfc3339(),
        };
        serde_json::to_string(&payload).map_err(|e| anyhow::anyhow!(e))
    }

    pub fn responsibility_comment(&self, candidate: &RoutingCandidate) -> anyhow::Result<String> {
        let payload = ResponsibilityCommentEnvelope {
            kind: "workgraph.routing-responsibility/v1".to_string(),
            decision_id: self.decision_id.clone(),
            responsibility_type: self.next_responsibility_type.clone(),
            owner: self.next_responsibility_owner.clone(),
            marker_request: self.marker_request.clone(),
            route_id: candidate.route_id.clone(),
            responsibility_id: candidate.responsibility_id.clone(),
            created_at: Utc::now().to_rfc3339(),
        };
        serde_json::to_string(&payload).map_err(|e| anyhow::anyhow!(e))
    }
}

pub fn deterministic_decision_id(
    config: &WorkgraphRouterReactionConfig,
    candidate: &RoutingCandidate,
    policy: &PolicyOutcome,
) -> Uuid {
    let input = format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        candidate.execution_id,
        candidate.required_event_type,
        candidate.event_id,
        config.policy_id,
        config.policy_type,
        config.policy_version,
        policy.from_status,
        policy.to_status,
        policy.next_responsibility_type,
        candidate.outcome_key().to_ascii_lowercase()
    );
    Uuid::new_v5(&DECISION_NAMESPACE, input.as_bytes())
}
