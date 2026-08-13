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
            && has_trusted_decision_comment(&comments, decision, &candidate.event_id, config)
        {
            progress.decision_comment_written = true;
        }

        if !progress.responsibility_written
            && has_trusted_responsibility_comment(&comments, decision, config)
        {
            progress.responsibility_written = true;
        }
    }

    if !progress.project_status_updated {
        let project_status = github
            .current_project_status(
                &candidate.project_id,
                &candidate.project_item_id,
                &candidate.subject_repo,
                candidate.subject_issue_number,
            )
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
    config: &WorkgraphRouterReactionConfig,
) -> bool {
    comments.iter().any(|comment| {
        if !is_trusted_reconciliation_comment_author(comment, config) || !comment.is_unedited() {
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
    config: &WorkgraphRouterReactionConfig,
) -> bool {
    comments.iter().any(|comment| {
        if !is_trusted_reconciliation_comment_author(comment, config) || !comment.is_unedited() {
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

fn is_trusted_reconciliation_comment_author(
    comment: &crate::github_client::IssueComment,
    config: &WorkgraphRouterReactionConfig,
) -> bool {
    if !config
        .trusted_router_authors
        .iter()
        .any(|author| author == &comment.author_login)
    {
        return false;
    }

    if !config.strict_recovery {
        return true;
    }

    let has_immutable_allowlist = !config.trusted_router_author_node_ids.is_empty()
        || !config.trusted_router_author_database_ids.is_empty();
    if !has_immutable_allowlist {
        return false;
    }

    let node_match = comment.author_node_id.as_ref().is_some_and(|node_id| {
        config
            .trusted_router_author_node_ids
            .iter()
            .any(|trusted| trusted == node_id)
    });
    let db_match = comment.author_database_id.is_some_and(|database_id| {
        config
            .trusted_router_author_database_ids
            .contains(&database_id)
    });
    node_match || db_match
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StatusTransition;
    use crate::rules::{RoutingPolicyEngine, RulesV1PolicyEngine};

    fn sample_candidate() -> RoutingCandidate {
        RoutingCandidate {
            execution_id: "exec-1".to_string(),
            required_event_type: "CompletedIssueValidation".to_string(),
            event_id: "event-1".to_string(),
            event_type: "CompletedIssueValidation".to_string(),
            outcome: "passed".to_string(),
            subject_repo: "drasi-project/drasi-core".to_string(),
            subject_issue_number: 42,
            project_id: "PVT_project".to_string(),
            project_item_id: "PVTI_item".to_string(),
            project_status: "AwaitingRouting".to_string(),
            route_id: "route-1".to_string(),
            route_expected_event_id: "event-1".to_string(),
            route_expected_event_type: "CompletedIssueValidation".to_string(),
            route_expected_subject_repo: "drasi-project/drasi-core".to_string(),
            route_expected_subject_issue_number: 42,
            route_content_version: "sha256:abc".to_string(),
            route_content_profile: "phase2".to_string(),
            responsibility_id: "resp-1".to_string(),
            responsibility_type: "issue-validation".to_string(),
            responsibility_actor: "bot-user".to_string(),
            submitter_actor: "submitter-user".to_string(),
            launcher_author: "launcher-user".to_string(),
            agent_author: "agent-user".to_string(),
            router_author: "router-user".to_string(),
            routing_author: "router-user".to_string(),
            observed_authors: vec![
                "launcher-user".to_string(),
                "agent-user".to_string(),
                "router-user".to_string(),
            ],
            comment_id: 1,
            comment_author: "router-user".to_string(),
            comment_body: "{\"ok\":true}".to_string(),
            comment_edited: false,
            comment_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            comment_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            comment_provenance_event_id: "event-1".to_string(),
            comment_provenance_event_type: "CompletedIssueValidation".to_string(),
            content_version: "sha256:abc".to_string(),
            content_profile: "phase2".to_string(),
        }
    }

    fn sample_config() -> WorkgraphRouterReactionConfig {
        WorkgraphRouterReactionConfig {
            policy_id: "policy-1".to_string(),
            policy_type: "rules_v1".to_string(),
            policy_version: "1.0.0".to_string(),
            allowed_projects: vec!["PVT_project".to_string()],
            allowed_repos: vec!["drasi-project/drasi-core".to_string()],
            allowed_event_types: vec!["CompletedIssueValidation".to_string()],
            allowed_status_transitions: vec![
                StatusTransition {
                    from: "AwaitingRouting".to_string(),
                    to: "AwaitingIssueRiskProfiling".to_string(),
                },
                StatusTransition {
                    from: "AwaitingRouting".to_string(),
                    to: "NeedsMoreInformation".to_string(),
                },
            ],
            allowed_responsibility_types: vec![
                "issue-validation".to_string(),
                "issue-risk-profiling".to_string(),
                "issue-correction".to_string(),
            ],
            allowed_actors: vec!["bot-user".to_string(), "submitter-user".to_string()],
            trusted_routing_authors: vec!["router-user".to_string()],
            trusted_launcher_authors: vec!["launcher-user".to_string()],
            trusted_agent_authors: vec!["agent-user".to_string()],
            trusted_router_authors: vec!["router-user".to_string()],
            timeout_secs: 5,
            reservation_lease_secs: 15,
            strict_recovery: true,
            ..WorkgraphRouterReactionConfig::default()
        }
    }

    fn sample_decision(
        config: &WorkgraphRouterReactionConfig,
        candidate: &RoutingCandidate,
    ) -> RoutingDecision {
        let policy = RulesV1PolicyEngine
            .evaluate(candidate)
            .expect("rules evaluation");
        RoutingDecision::from_policy(config, candidate, policy).expect("decision")
    }

    fn decision_comment(
        decision: &RoutingDecision,
        candidate: &RoutingCandidate,
        author_login: &str,
        author_node_id: Option<&str>,
    ) -> crate::github_client::IssueComment {
        crate::github_client::IssueComment {
            id: 1,
            body: decision
                .decision_comment(candidate)
                .expect("decision payload"),
            author_login: author_login.to_string(),
            author_node_id: author_node_id.map(ToString::to_string),
            author_database_id: Some(1001),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            updated_at: Some("2026-01-01T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn reconciliation_rejects_same_login_wrong_node_id() {
        let candidate = sample_candidate();
        let mut config = sample_config();
        config.trusted_router_author_node_ids = vec!["MDQ6VXNlcjE=".to_string()];
        let decision = sample_decision(&config, &candidate);
        let comments = vec![decision_comment(
            &decision,
            &candidate,
            "router-user",
            Some("MDQ6VXNlcjk5"),
        )];

        assert!(!has_trusted_decision_comment(
            &comments,
            &decision,
            &candidate.event_id,
            &config
        ));
    }

    #[test]
    fn reconciliation_accepts_trusted_immutable_id() {
        let candidate = sample_candidate();
        let mut config = sample_config();
        config.trusted_router_author_node_ids = vec!["MDQ6VXNlcjE=".to_string()];
        let decision = sample_decision(&config, &candidate);
        let comments = vec![decision_comment(
            &decision,
            &candidate,
            "router-user",
            Some("MDQ6VXNlcjE="),
        )];

        assert!(has_trusted_decision_comment(
            &comments,
            &decision,
            &candidate.event_id,
            &config
        ));
    }

    #[test]
    fn reconciliation_falls_back_to_login_when_not_strict() {
        let candidate = sample_candidate();
        let mut config = sample_config();
        config.strict_recovery = false;
        let decision = sample_decision(&config, &candidate);
        let comments = vec![decision_comment(&decision, &candidate, "router-user", None)];

        assert!(has_trusted_decision_comment(
            &comments,
            &decision,
            &candidate.event_id,
            &config
        ));
    }
}
