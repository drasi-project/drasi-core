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

use serde_json::Value;

use crate::candidate::RoutingCandidate;
use crate::config::WorkgraphRouterReactionConfig;
use crate::decision::{DecisionCommentEnvelope, ResponsibilityCommentEnvelope, RoutingDecision};
use crate::github_client::GithubClient;
use crate::state::SideEffectProgress;

pub async fn reconcile_progress(
    github: &GithubClient,
    candidate: &RoutingCandidate,
    decision: &RoutingDecision,
    config: &WorkgraphRouterReactionConfig,
    mut progress: SideEffectProgress,
) -> anyhow::Result<SideEffectProgress> {
    if !progress.decision_comment_written || !progress.responsibility_written {
        let comments = github
            .list_issue_comments(&candidate.subject_repo, candidate.subject_issue_number)
            .await?;

        if !progress.decision_comment_written
            && has_trusted_decision_comment(
                &comments,
                decision,
                &candidate.event_id,
                &config.trusted_router_authors,
            )
        {
            progress.decision_comment_written = true;
        }

        if !progress.responsibility_written
            && has_trusted_responsibility_comment(
                &comments,
                decision,
                &config.trusted_router_authors,
            )
        {
            progress.responsibility_written = true;
        }
    }

    if !progress.project_status_updated {
        let project_status = github
            .current_project_status(&candidate.project_item_id)
            .await?;
        if project_status == decision.to_status {
            progress.project_status_updated = true;
        }
    }

    Ok(progress)
}

fn has_trusted_decision_comment(
    comments: &[crate::github_client::IssueComment],
    decision: &RoutingDecision,
    source_event_id: &str,
    trusted_authors: &[String],
) -> bool {
    comments.iter().any(|comment| {
        if !trusted_authors
            .iter()
            .any(|author| author == &comment.author_login)
            || !comment.is_unedited()
        {
            return false;
        }
        let Ok(payload) = serde_json::from_str::<DecisionCommentEnvelope>(&comment.body) else {
            return false;
        };
        payload.kind == "workgraph.routing-decision/v1"
            && payload.decision_id == decision.decision_id
            && payload.source_event.event_id == source_event_id
            && payload.policy.id == decision.policy_id
            && payload.policy.version == decision.policy_version
    })
}

fn has_trusted_responsibility_comment(
    comments: &[crate::github_client::IssueComment],
    decision: &RoutingDecision,
    trusted_authors: &[String],
) -> bool {
    comments.iter().any(|comment| {
        if !trusted_authors
            .iter()
            .any(|author| author == &comment.author_login)
            || !comment.is_unedited()
        {
            return false;
        }
        let Ok(payload) = serde_json::from_str::<ResponsibilityCommentEnvelope>(&comment.body)
        else {
            return false;
        };
        payload.kind == "workgraph.routing-responsibility/v1"
            && payload.decision_id == decision.decision_id
            && payload.responsibility_type == decision.next_responsibility_type
    })
}

pub fn body_has_type(body: &str, expected: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(|s| s == expected))
        .unwrap_or(false)
}
