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

use std::collections::HashSet;

use crate::candidate::RoutingCandidate;
use crate::config::WorkgraphRouterReactionConfig;

pub fn validate_candidate(
    candidate: &RoutingCandidate,
    config: &WorkgraphRouterReactionConfig,
) -> anyhow::Result<()> {
    ensure_not_empty("executionId", &candidate.execution_id)?;
    ensure_not_empty("requiredEventType", &candidate.required_event_type)?;
    ensure_not_empty("eventId", &candidate.event_id)?;
    ensure_not_empty("eventType", &candidate.event_type)?;
    ensure_not_empty("reasonCode", &candidate.reason_code)?;
    ensure_not_empty("eventNodeId", &candidate.event_node_id)?;
    ensure_not_empty("subjectRepo", &candidate.subject_repo)?;
    ensure_not_empty("subjectNodeId", &candidate.subject_node_id)?;
    ensure_not_empty("projectId", &candidate.project_id)?;
    ensure_not_empty("projectItemId", &candidate.project_item_id)?;
    ensure_not_empty("routeId", &candidate.route_id)?;
    ensure_not_empty("responsibilityId", &candidate.responsibility_id)?;
    ensure_not_empty("contentVersion", &candidate.content_version)?;
    ensure_not_empty("contentProfile", &candidate.content_profile)?;
    ensure_not_empty("policyId", &candidate.policy_id)?;
    ensure_not_empty("policyType", &candidate.policy_type)?;
    ensure_not_empty("policyVersion", &candidate.policy_version)?;
    ensure_not_empty("routingAuthor", &candidate.routing_author)?;
    ensure_not_empty("launcherAuthor", &candidate.launcher_author)?;
    ensure_not_empty("agentAuthor", &candidate.agent_author)?;
    ensure_not_empty("routerAuthor", &candidate.router_author)?;
    ensure_not_empty("commentAuthor", &candidate.comment_author)?;

    if candidate.comment_id == 0 || candidate.subject_issue_number == 0 {
        anyhow::bail!("commentId and subjectIssueNumber must be positive");
    }
    if !candidate.event_node_id.starts_with("workgraph-event:") {
        anyhow::bail!("eventNodeId must start with 'workgraph-event:'");
    }
    if candidate.policy_id != config.policy_id
        || candidate.policy_type != config.policy_type
        || candidate.policy_version != config.policy_version
    {
        anyhow::bail!(
            "candidate policy {}:{}@{} does not match configured policy {}:{}@{}",
            candidate.policy_id,
            candidate.policy_type,
            candidate.policy_version,
            config.policy_id,
            config.policy_type,
            config.policy_version
        );
    }
    match candidate.outcome_key() {
        "passed" if candidate.reason_code == "required-marker-present" => {}
        "failed" if candidate.reason_code == "required-marker-missing" => {}
        _ => anyhow::bail!(
            "outcome '{}' and reasonCode '{}' are inconsistent",
            candidate.outcome,
            candidate.reason_code
        ),
    }

    require_allowlisted(
        "requiredEventType",
        &candidate.required_event_type,
        &config.allowed_event_types,
    )?;
    require_allowlisted(
        "subjectRepo",
        &candidate.subject_repo,
        &config.allowed_repos,
    )?;
    require_allowlisted("projectId", &candidate.project_id, &config.allowed_projects)?;
    require_allowlisted(
        "responsibilityType",
        &candidate.responsibility_type,
        &config.allowed_responsibility_types,
    )?;

    if !config.allowed_actors.is_empty() {
        require_allowlisted(
            "responsibilityActor",
            &candidate.responsibility_actor,
            &config.allowed_actors,
        )?;
        require_allowlisted(
            "submitterActor",
            &candidate.submitter_actor,
            &config.allowed_actors,
        )?;
    }

    if candidate.event_type != candidate.required_event_type {
        anyhow::bail!(
            "eventType '{}' does not match requiredEventType '{}'",
            candidate.event_type,
            candidate.required_event_type
        );
    }
    if candidate.event_id != candidate.route_expected_event_id {
        anyhow::bail!(
            "eventId '{}' does not match routeExpectedEventId '{}'",
            candidate.event_id,
            candidate.route_expected_event_id
        );
    }
    if candidate.event_type != candidate.route_expected_event_type {
        anyhow::bail!(
            "eventType '{}' does not match routeExpectedEventType '{}'",
            candidate.event_type,
            candidate.route_expected_event_type
        );
    }
    if candidate.subject_repo != candidate.route_expected_subject_repo {
        anyhow::bail!(
            "subjectRepo '{}' does not match routeExpectedSubjectRepo '{}'",
            candidate.subject_repo,
            candidate.route_expected_subject_repo
        );
    }
    if candidate.subject_issue_number != candidate.route_expected_subject_issue_number {
        anyhow::bail!(
            "subjectIssueNumber '{}' does not match routeExpectedSubjectIssueNumber '{}'",
            candidate.subject_issue_number,
            candidate.route_expected_subject_issue_number
        );
    }
    if candidate.content_version != candidate.route_content_version {
        anyhow::bail!(
            "contentVersion '{}' does not match routeContentVersion '{}'",
            candidate.content_version,
            candidate.route_content_version
        );
    }
    if candidate.content_profile != candidate.route_content_profile {
        anyhow::bail!(
            "contentProfile '{}' does not match routeContentProfile '{}'",
            candidate.content_profile,
            candidate.route_content_profile
        );
    }
    if candidate.event_id != candidate.comment_provenance_event_id {
        anyhow::bail!(
            "eventId '{}' does not match commentProvenanceEventId '{}'",
            candidate.event_id,
            candidate.comment_provenance_event_id
        );
    }
    if candidate.event_type != candidate.comment_provenance_event_type {
        anyhow::bail!(
            "eventType '{}' does not match commentProvenanceEventType '{}'",
            candidate.event_type,
            candidate.comment_provenance_event_type
        );
    }
    if candidate.comment_edited {
        anyhow::bail!("source comment provenance is edited; unedited comment is required");
    }

    require_trusted_id(
        "routingAuthorId",
        candidate.routing_author_id,
        &config.trusted_routing_user_ids,
    )?;
    require_trusted_id(
        "launcherAuthorId",
        candidate.launcher_author_id,
        &config.trusted_launcher_user_ids,
    )?;
    require_trusted_id(
        "agentAuthorId",
        candidate.agent_author_id,
        &config.trusted_agent_user_ids,
    )?;
    require_trusted_id(
        "commentAuthorId",
        candidate.agent_author_id,
        &config.trusted_agent_user_ids,
    )?;
    require_trusted_id(
        "routerAuthorId",
        candidate.router_author_id,
        &config.trusted_router_user_ids,
    )?;

    if candidate.comment_author != candidate.agent_author {
        anyhow::bail!("commentAuthor does not match agentAuthor");
    }

    let expected_observed_authors = [
        candidate.routing_author.as_str(),
        candidate.launcher_author.as_str(),
        candidate.agent_author.as_str(),
    ];
    if candidate
        .observed_authors
        .iter()
        .map(String::as_str)
        .ne(expected_observed_authors)
    {
        anyhow::bail!("observedAuthors does not match routing, launcher, and agent provenance");
    }
    let expected_observed_author_ids = [
        candidate.routing_author_id,
        candidate.launcher_author_id,
        candidate.agent_author_id,
    ];
    if candidate
        .observed_author_ids
        .iter()
        .copied()
        .ne(expected_observed_author_ids)
    {
        anyhow::bail!("observedAuthorIds does not match routing, launcher, and agent provenance");
    }

    Ok(())
}

fn ensure_not_empty(name: &str, value: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{name} is required");
    }
    Ok(())
}

fn require_allowlisted(name: &str, value: &str, allowlist: &[String]) -> anyhow::Result<()> {
    if allowlist.iter().any(|allowed| allowed == value) {
        return Ok(());
    }
    anyhow::bail!("{name} '{value}' is not in allowlist");
}

fn require_trusted_id(name: &str, value: u64, allowlist: &[u64]) -> anyhow::Result<()> {
    if value > 0 && allowlist.contains(&value) {
        return Ok(());
    }
    anyhow::bail!("{name} '{value}' is not trusted");
}

pub fn trusted_author_set(config: &WorkgraphRouterReactionConfig) -> HashSet<String> {
    config.trusted_observed_authors()
}
