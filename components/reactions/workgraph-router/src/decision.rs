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

use crate::candidate::RoutingCandidate;
use crate::config::WorkgraphRouterReactionConfig;
use crate::rules::PolicyOutcome;

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
    pub reason_code: String,
    pub decided_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DecisionCommentEnvelope {
    pub schema_version: String,
    pub message_type: String,
    pub decision_id: String,
    pub source_event_node_id: String,
    pub source_event_id: String,
    pub project_item_node_id: String,
    pub subject_node_id: String,
    pub policy_id: String,
    pub policy_type: String,
    pub policy_version: String,
    pub from_status: String,
    pub to_status: String,
    pub next_responsibility_type: String,
    pub reason_code: String,
    pub decided_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
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

impl RoutingDecision {
    pub fn validate_allowlists(
        &self,
        config: &WorkgraphRouterReactionConfig,
    ) -> anyhow::Result<()> {
        if !config
            .allowed_responsibility_types
            .iter()
            .any(|allowed| allowed == &self.next_responsibility_type)
        {
            anyhow::bail!(
                "policy output nextResponsibilityType '{}' is not in allowedResponsibilityTypes",
                self.next_responsibility_type
            );
        }
        if !config.allowed_actors.is_empty()
            && !config
                .allowed_actors
                .iter()
                .any(|allowed| allowed == &self.next_responsibility_owner)
        {
            anyhow::bail!(
                "policy output nextResponsibilityOwner '{}' is not in allowedActors",
                self.next_responsibility_owner
            );
        }
        Ok(())
    }

    pub fn from_policy(
        config: &WorkgraphRouterReactionConfig,
        candidate: &RoutingCandidate,
        policy: PolicyOutcome,
    ) -> anyhow::Result<Self> {
        let decision_id = deterministic_decision_id(candidate);
        let decision = Self {
            decision_id,
            policy_id: config.policy_id.clone(),
            policy_type: config.policy_type.clone(),
            policy_version: config.policy_version.clone(),
            from_status: policy.from_status,
            to_status: policy.to_status,
            next_responsibility_type: policy.next_responsibility_type,
            next_responsibility_owner: policy.next_responsibility_owner,
            marker_request: policy.marker_request,
            reason_code: policy.reason_code,
            decided_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        decision.validate_allowlists(config)?;
        Ok(decision)
    }

    pub fn decision_comment(&self, candidate: &RoutingCandidate) -> anyhow::Result<String> {
        let payload = DecisionCommentEnvelope {
            schema_version: "workgraph.routing-decision/v1".to_string(),
            message_type: "routing-decision".to_string(),
            decision_id: self.decision_id.clone(),
            source_event_node_id: candidate.event_node_id.clone(),
            source_event_id: candidate.event_id.clone(),
            project_item_node_id: candidate.project_item_id.clone(),
            subject_node_id: candidate.subject_node_id.clone(),
            policy_id: self.policy_id.clone(),
            policy_type: self.policy_type.clone(),
            policy_version: self.policy_version.clone(),
            from_status: self.from_status.clone(),
            to_status: self.to_status.clone(),
            next_responsibility_type: self.next_responsibility_type.clone(),
            reason_code: self.reason_code.clone(),
            decided_at: self.decided_at.clone(),
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
            created_at: self.decided_at.clone(),
        };
        serde_json::to_string(&payload).map_err(|e| anyhow::anyhow!(e))
    }
}

pub fn deterministic_decision_id(candidate: &RoutingCandidate) -> String {
    format!("decision:{}", candidate.event_id)
}
