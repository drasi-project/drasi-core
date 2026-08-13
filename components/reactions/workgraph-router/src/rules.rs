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

use crate::candidate::RoutingCandidate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    RulesV1,
    Linear,
    Llm,
}

impl TryFrom<&str> for PolicyMode {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_ascii_lowercase().as_str() {
            "rules" | "rules_v1" => Ok(Self::RulesV1),
            "linear" => Ok(Self::Linear),
            "llm" => Ok(Self::Llm),
            other => anyhow::bail!("unsupported policyType '{other}'"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyOutcome {
    pub from_status: String,
    pub to_status: String,
    pub next_responsibility_type: String,
    pub next_responsibility_owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_request: Option<String>,
    pub reason: String,
}

pub trait RoutingPolicyEngine: Send + Sync {
    fn evaluate(&self, candidate: &RoutingCandidate) -> anyhow::Result<PolicyOutcome>;
}

#[derive(Debug, Default)]
pub struct RulesV1PolicyEngine;

impl RoutingPolicyEngine for RulesV1PolicyEngine {
    fn evaluate(&self, candidate: &RoutingCandidate) -> anyhow::Result<PolicyOutcome> {
        if candidate.required_event_type != "CompletedIssueValidation"
            || candidate.event_type != "CompletedIssueValidation"
        {
            anyhow::bail!(
                "rules_v1 only supports CompletedIssueValidation; got required='{}' event='{}'",
                candidate.required_event_type,
                candidate.event_type
            );
        }

        match candidate.outcome_key().to_ascii_lowercase().as_str() {
            "passed" => Ok(PolicyOutcome {
                from_status: "AwaitingRouting".to_string(),
                to_status: "AwaitingIssueRiskProfiling".to_string(),
                next_responsibility_type: "issue-risk-profiling".to_string(),
                next_responsibility_owner: candidate.responsibility_actor.clone(),
                marker_request: None,
                reason: "Completed issue validation passed; route to issue risk profiling."
                    .to_string(),
            }),
            "failed" => Ok(PolicyOutcome {
                from_status: "AwaitingRouting".to_string(),
                to_status: "NeedsMoreInformation".to_string(),
                next_responsibility_type: "issue-correction".to_string(),
                next_responsibility_owner: candidate.submitter_actor.clone(),
                marker_request: Some(
                    "Please provide additional details requested by issue validation.".to_string(),
                ),
                reason: "Completed issue validation failed; request submitter correction."
                    .to_string(),
            }),
            other => anyhow::bail!("unsupported outcome '{other}' for rules_v1"),
        }
    }
}
