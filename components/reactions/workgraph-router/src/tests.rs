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

use crate::candidate::RoutingCandidate;
use crate::config::{StatusTransition, WorkgraphRouterReactionConfig};
use crate::decision::{deterministic_decision_id, RoutingDecision};
use crate::github_client::GithubClient;
use crate::rules::{RoutingPolicyEngine, RulesV1PolicyEngine};
use crate::validation::validate_candidate;

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
        comment_id: 999,
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
        ..WorkgraphRouterReactionConfig::default()
    }
}

fn github_client_for_test(
    server: &wiremock::MockServer,
    token_env: &str,
    token_value: &str,
) -> GithubClient {
    std::env::set_var(token_env, token_value);
    let config = WorkgraphRouterReactionConfig {
        policy_id: "policy".to_string(),
        policy_type: "rules_v1".to_string(),
        policy_version: "1.0.0".to_string(),
        github_rest_url: server.uri(),
        github_graphql_url: format!("{}/graphql", server.uri()),
        github_token_env: token_env.to_string(),
        ..sample_config()
    };
    GithubClient::from_config(&config).expect("client")
}

#[test]
fn rules_v1_pass_routes_to_risk_profiling() {
    let candidate = sample_candidate();
    let policy = RulesV1PolicyEngine
        .evaluate(&candidate)
        .expect("rules evaluation");
    assert_eq!(policy.to_status, "AwaitingIssueRiskProfiling");
    assert_eq!(policy.next_responsibility_type, "issue-risk-profiling");
}

#[test]
fn rules_v1_failed_routes_to_issue_correction() {
    let mut candidate = sample_candidate();
    candidate.outcome = "failed".to_string();
    let policy = RulesV1PolicyEngine
        .evaluate(&candidate)
        .expect("rules evaluation");
    assert_eq!(policy.to_status, "NeedsMoreInformation");
    assert_eq!(policy.next_responsibility_type, "issue-correction");
    assert_eq!(policy.next_responsibility_owner, "submitter-user");
    assert!(policy.marker_request.is_some());
}

#[test]
fn decision_id_is_deterministic() {
    let config = sample_config();
    let candidate = sample_candidate();
    let policy = RulesV1PolicyEngine
        .evaluate(&candidate)
        .expect("rules evaluation");
    let first = deterministic_decision_id(&config, &candidate, &policy);
    let second = deterministic_decision_id(&config, &candidate, &policy);
    assert_eq!(first, second);
}

#[test]
fn validation_rejects_untrusted_observed_author() {
    let mut candidate = sample_candidate();
    candidate.observed_authors.push("mallory".to_string());
    let err = validate_candidate(&candidate, &sample_config()).expect_err("validation should fail");
    assert!(err.to_string().contains("untrusted"));
}

#[test]
fn validation_rejects_stale_content_version() {
    let mut candidate = sample_candidate();
    candidate.content_version = "sha256:def".to_string();
    let err = validate_candidate(&candidate, &sample_config()).expect_err("validation should fail");
    assert!(err.to_string().contains("contentVersion"));
}

#[test]
fn decision_comment_is_pure_json() {
    let config = sample_config();
    let candidate = sample_candidate();
    let policy = RulesV1PolicyEngine
        .evaluate(&candidate)
        .expect("rules evaluation");
    let decision = RoutingDecision::from_policy(&config, &candidate, policy).expect("decision");
    let comment = decision
        .decision_comment(&candidate)
        .expect("decision comment");
    let payload: serde_json::Value = serde_json::from_str(&comment).expect("valid json");
    assert_eq!(
        payload.get("type").and_then(|v| v.as_str()),
        Some("workgraph.routing-decision/v1")
    );
}

#[tokio::test]
async fn github_issue_preflight_sends_bearer_authorization() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let token = "token-issue-open";
    let expected_auth = format!("{} {}", "Bearer", token);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .and(header("authorization", expected_auth.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "state":"open"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = github_client_for_test(&server, "WG_ROUTER_TEST_TOKEN_ISSUE_OPEN", token);
    let is_open = client
        .issue_is_open("drasi-project/drasi-core", 42)
        .await
        .expect("issue preflight");
    assert!(is_open);
}

#[tokio::test]
async fn github_create_comment_sends_bearer_authorization() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let token = "token-comment-create";
    let expected_auth = format!("{} {}", "Bearer", token);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .and(header("authorization", expected_auth.as_str()))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 1,
            "body": "ok",
            "user": {"login":"router-user"}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = github_client_for_test(&server, "WG_ROUTER_TEST_TOKEN_CREATE_COMMENT", token);
    let comment = client
        .create_issue_comment("drasi-project/drasi-core", 42, "{\"ok\":true}")
        .await
        .expect("create comment");
    assert_eq!(comment.id, 1);
}

#[tokio::test]
async fn github_list_comments_sends_bearer_authorization() {
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let token = "token-list-comments";
    let expected_auth = format!("{} {}", "Bearer", token);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "1"))
        .and(header("authorization", expected_auth.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let client = github_client_for_test(&server, "WG_ROUTER_TEST_TOKEN_LIST_COMMENTS", token);
    let comments = client
        .list_issue_comments("drasi-project/drasi-core", 42)
        .await
        .expect("list comments");
    assert!(comments.is_empty());
}

#[tokio::test]
async fn github_graphql_sends_bearer_authorization() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let token = "token-graphql";
    let expected_auth = format!("{} {}", "Bearer", token);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", expected_auth.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"viewer": {"login": "octocat"}}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = github_client_for_test(&server, "WG_ROUTER_TEST_TOKEN_GRAPHQL", token);
    let data = client
        .graphql("query Q { viewer { login } }", serde_json::json!({}))
        .await
        .expect("graphql");
    assert_eq!(
        data.pointer("/viewer/login").and_then(|v| v.as_str()),
        Some("octocat")
    );
}

#[test]
fn policy_output_rejected_when_next_responsibility_type_not_allowlisted() {
    let mut config = sample_config();
    config.allowed_responsibility_types = vec!["issue-validation".to_string()];
    let candidate = sample_candidate();
    let policy = RulesV1PolicyEngine
        .evaluate(&candidate)
        .expect("rules evaluation");
    let err = RoutingDecision::from_policy(&config, &candidate, policy)
        .expect_err("decision should reject policy output type");
    assert!(
        err.to_string()
            .contains("nextResponsibilityType 'issue-risk-profiling'"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn policy_output_rejected_when_issue_correction_not_allowlisted() {
    let mut config = sample_config();
    config.allowed_responsibility_types = vec!["issue-validation".to_string()];
    let mut candidate = sample_candidate();
    candidate.outcome = "failed".to_string();
    let policy = RulesV1PolicyEngine
        .evaluate(&candidate)
        .expect("rules evaluation");
    let err = RoutingDecision::from_policy(&config, &candidate, policy)
        .expect_err("decision should reject issue-correction output type");
    assert!(
        err.to_string()
            .contains("nextResponsibilityType 'issue-correction'"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn policy_output_rejected_when_owner_not_allowlisted() {
    let mut config = sample_config();
    config.allowed_actors = vec!["submitter-user".to_string()];
    let candidate = sample_candidate();
    let policy = RulesV1PolicyEngine
        .evaluate(&candidate)
        .expect("rules evaluation");
    let err = RoutingDecision::from_policy(&config, &candidate, policy)
        .expect_err("decision should reject policy output owner");
    assert!(
        err.to_string()
            .contains("nextResponsibilityOwner 'bot-user'"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn github_errors_do_not_leak_token_value() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors":[{"message":"bad"}],
            "data": null
        })))
        .mount(&server)
        .await;

    let token_value = "super-secret-token-value";
    std::env::set_var("WG_ROUTER_TEST_TOKEN_REDACTION", token_value);

    let config = WorkgraphRouterReactionConfig {
        policy_id: "policy".to_string(),
        policy_type: "rules_v1".to_string(),
        policy_version: "1.0.0".to_string(),
        github_rest_url: server.uri(),
        github_graphql_url: format!("{}/graphql", server.uri()),
        github_token_env: "WG_ROUTER_TEST_TOKEN_REDACTION".to_string(),
        ..sample_config()
    };
    let client = GithubClient::from_config(&config).expect("client");
    let error = client
        .graphql("query Q { viewer { login } }", serde_json::json!({}))
        .await
        .expect_err("graphql should fail");
    let rendered = format!("{error:#}");
    assert!(
        !rendered.contains(token_value),
        "error output leaked token value"
    );
}

#[tokio::test]
async fn github_transport_errors_do_not_leak_token_value() {
    let token_value = "transport-secret-token";
    std::env::set_var("WG_ROUTER_TEST_TOKEN_TRANSPORT_REDACTION", token_value);
    let config = WorkgraphRouterReactionConfig {
        policy_id: "policy".to_string(),
        policy_type: "rules_v1".to_string(),
        policy_version: "1.0.0".to_string(),
        github_rest_url: "http://127.0.0.1:1".to_string(),
        github_graphql_url: "http://127.0.0.1:1/graphql".to_string(),
        github_token_env: "WG_ROUTER_TEST_TOKEN_TRANSPORT_REDACTION".to_string(),
        ..sample_config()
    };
    let client = GithubClient::from_config(&config).expect("client");
    let error = client
        .issue_is_open("drasi-project/drasi-core", 42)
        .await
        .expect_err("transport should fail");
    let rendered = format!("{error:#}");
    assert!(
        !rendered.contains(token_value),
        "transport error output leaked token value"
    );
}
