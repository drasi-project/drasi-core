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

//! GitHub WorkGraph bootstrap provider.
//!
//! Enumerates every repository in one configured GitHub organization over the
//! GitHub GraphQL v4 API, and snapshots each repository's currently-open
//! Issues and Pull Requests, their conversation comments, and PR reviews.
//!
//! # Reuse, not duplication
//!
//! This crate never re-implements any WorkGraph domain rule. Every node
//! label, relation ID/direction, status-label derivation, and
//! `WorkGraphAssignment`/`WorkGraphResult`/`WorkGraphError` parsing rule comes
//! from [`drasi_source_github_workgraph::mapping::Converter`] — the exact
//! same converter the streaming `drasi-source-github-workgraph` source uses
//! for live webhook deliveries. This crate's only job is to fetch GitHub data
//! over GraphQL and reshape it into the same JSON envelope shape a GitHub
//! webhook delivery would have (see [`client`] module docs), then hand that
//! envelope to `Converter` unchanged.
//!
//! # Scope (prototype)
//!
//! - One configured organization; all repositories the token can see.
//! - Only currently-**open** Issues and Pull Requests (no closed history).
//! - Issue/PR conversation comments and PR reviews only.
//! - Excluded: Projects, Project Items, inline diff comments, closed-item
//!   history, reactions, and workflow-run execution state.
//!
//! # `source_position` is always `None`
//!
//! The GitHub WorkGraph source is driven entirely by webhook deliveries,
//! which have no durable, replayable position analogous to a database WAL
//! LSN or a Kafka offset — a webhook delivery, once missed, cannot be
//! re-requested from GitHub by position. Because of this, [`BootstrapResult`]
//! from this provider always carries `source_position: None`; there is no
//! snapshot boundary for the framework to seed a replay checkpoint from.

mod client;
mod config;
pub mod descriptor;
#[cfg(test)]
mod tests;

pub use config::GitHubWorkGraphBootstrapConfig;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use client::GitHubGraphQLClient;
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_source_github_workgraph::mapping::{Converter, NODE_LABELS, RELATION_LABELS};
use log::{info, warn};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub struct GitHubWorkGraphBootstrapProvider {
    config: GitHubWorkGraphBootstrapConfig,
}

impl GitHubWorkGraphBootstrapProvider {
    pub fn builder() -> GitHubWorkGraphBootstrapProviderBuilder {
        GitHubWorkGraphBootstrapProviderBuilder::new()
    }
}

#[derive(Default)]
pub struct GitHubWorkGraphBootstrapProviderBuilder {
    config: GitHubWorkGraphBootstrapConfig,
}

impl GitHubWorkGraphBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            config: GitHubWorkGraphBootstrapConfig::default(),
        }
    }

    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.config.organization = organization.into();
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.config.token = token.into();
        self
    }

    pub fn with_api_base_url(mut self, api_base_url: impl Into<String>) -> Self {
        self.config.api_base_url = api_base_url.into();
        self
    }

    pub fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.config.max_concurrency = max_concurrency;
        self
    }

    pub fn build(self) -> Result<GitHubWorkGraphBootstrapProvider> {
        self.config.validate()?;
        Ok(GitHubWorkGraphBootstrapProvider {
            config: self.config,
        })
    }
}

#[async_trait]
impl BootstrapProvider for GitHubWorkGraphBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        info!(
            "[{}] GitHub WorkGraph bootstrap started for org '{}', query {}",
            context.source_id, self.config.organization, request.query_id
        );

        let client = GitHubGraphQLClient::new(
            &self.config.token,
            &self.config.api_base_url,
            self.config.max_concurrency,
        )?;

        let org_value = client.fetch_organization(&self.config.organization).await?;
        let repos = client.fetch_repositories(&self.config.organization).await?;
        info!(
            "[{}] GitHub WorkGraph bootstrap: {} repositories in '{}'",
            context.source_id,
            repos.len(),
            self.config.organization
        );

        // Fan out one task per repository, bounded to `max_concurrency`
        // concurrently-running repository tasks. The GraphQL client itself
        // additionally bounds the number of concurrently in-flight HTTP
        // requests to the same limit, so total request concurrency stays
        // bounded even though each repository task issues several requests
        // in sequence (issues, PRs, then per-item comments/reviews).
        let semaphore = Arc::new(Semaphore::new(self.config.max_concurrency.max(1)));
        let mut join_set = JoinSet::new();
        for repo in repos {
            let client = client.clone();
            let organization = self.config.organization.clone();
            let source_id = context.source_id.clone();
            let org_value = org_value.clone();
            let semaphore = semaphore.clone();
            join_set.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow!("bootstrap concurrency semaphore closed"))?;
                process_repository(&client, &organization, &source_id, &org_value, repo).await
            });
        }

        let mut all_changes: Vec<SourceChange> = Vec::new();
        let mut repo_errors = Vec::new();
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok(Ok(changes)) => all_changes.extend(changes),
                Ok(Err(err)) => {
                    repo_errors.push(format!("{err:#}"));
                    warn!(
                        "[{}] GitHub WorkGraph bootstrap: repository task failed: {err:#}",
                        context.source_id
                    );
                }
                Err(join_err) => {
                    repo_errors.push(join_err.to_string());
                    warn!(
                        "[{}] GitHub WorkGraph bootstrap: repository task panicked: {join_err}",
                        context.source_id
                    );
                }
            }
        }
        if let Some(first_error) = repo_errors.first() {
            return Err(anyhow!(
                "GitHub WorkGraph bootstrap failed for {} repository task(s); first error: \
                 {first_error}",
                repo_errors.len()
            ));
        }

        // Filtering, dedup, and event sending happen sequentially here (after
        // all concurrent fetches have completed) so `event_count` and the
        // dedup/"first occurrence becomes Insert" logic below stay simple and
        // deterministic without needing a shared, lock-protected `HashSet`
        // across tasks.
        let mut sent_ids: HashSet<String> = HashSet::new();
        let mut event_count = 0usize;
        for change in all_changes {
            event_count += send_if_requested(
                change,
                &request,
                &context.source_id,
                &event_tx,
                context,
                &mut sent_ids,
            )
            .await?;
        }

        info!(
            "[{}] GitHub WorkGraph bootstrap complete for query {}: {event_count} event(s) sent",
            context.source_id, request.query_id
        );

        Ok(BootstrapResult {
            event_count,
            // Webhooks have no replay boundary to snapshot against (see the
            // module-level docs); this is always `None`.
            source_position: None,
        })
    }
}

/// Fetch and convert one repository's data into `SourceChange`s, entirely via
/// [`Converter`] (this function only assembles the synthetic webhook-shaped
/// JSON envelopes `Converter` expects; it never inspects or derives any
/// WorkGraph node/relation/status semantics itself).
async fn process_repository(
    client: &GitHubGraphQLClient,
    organization: &str,
    source_id: &str,
    org_value: &Value,
    repo_value: Value,
) -> Result<Vec<SourceChange>> {
    let owner = repo_value
        .pointer("/owner/login")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("repository missing owner.login"))?
        .to_string();
    let name = repo_value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("repository missing name"))?
        .to_string();

    let mut changes = Vec::new();

    changes.extend(convert(
        source_id,
        organization,
        "repository",
        "created",
        json!({ "organization": org_value, "repository": repo_value }),
    )?);

    let issues = client
        .fetch_issues(&owner, &name)
        .await
        .with_context(|| format!("failed to fetch issues for {owner}/{name}"))?;
    for issue in issues {
        let issue_node_id = required_node_id(&issue)?;
        changes.extend(convert(
            source_id,
            organization,
            "issues",
            "opened",
            json!({
                "organization": org_value,
                "repository": repo_value,
                "issue": issue,
            }),
        )?);

        if client::item_comment_count(&issue) > 0 {
            let comments = client
                .fetch_issue_comments(&issue_node_id)
                .await
                .with_context(|| {
                    format!("failed to fetch comments for issue {owner}/{name}#{issue_node_id}")
                })?;
            for comment in comments {
                changes.extend(convert(
                    source_id,
                    organization,
                    "issue_comment",
                    "created",
                    json!({
                        "organization": org_value,
                        "repository": repo_value,
                        "issue": { "node_id": issue_node_id },
                        "comment": comment,
                    }),
                )?);
            }
        }
    }

    let pull_requests = client
        .fetch_pull_requests(&owner, &name)
        .await
        .with_context(|| format!("failed to fetch pull requests for {owner}/{name}"))?;
    for pr in pull_requests {
        let pr_node_id = required_node_id(&pr)?;
        changes.extend(convert(
            source_id,
            organization,
            "pull_request",
            "opened",
            json!({
                "organization": org_value,
                "repository": repo_value,
                "pull_request": pr,
            }),
        )?);

        if client::item_comment_count(&pr) > 0 {
            let comments = client
                .fetch_pr_comments(&pr_node_id)
                .await
                .with_context(|| {
                    format!("failed to fetch comments for pull request {owner}/{name}#{pr_node_id}")
                })?;
            for comment in comments {
                changes.extend(convert(
                    source_id,
                    organization,
                    "issue_comment",
                    "created",
                    json!({
                        "organization": org_value,
                        "repository": repo_value,
                        // `pull_request` present (even empty) is exactly the
                        // discriminator `mapping::comment_event` checks via
                        // `issue.get("pull_request").is_some()`.
                        "issue": { "node_id": pr_node_id, "pull_request": {} },
                        "comment": comment,
                    }),
                )?);
            }
        }

        if client::pr_review_count(&pr) > 0 {
            let reviews = client
                .fetch_pr_reviews(&pr_node_id)
                .await
                .with_context(|| {
                    format!("failed to fetch reviews for pull request {owner}/{name}#{pr_node_id}")
                })?;
            for review in reviews {
                changes.extend(convert(
                    source_id,
                    organization,
                    "pull_request_review",
                    "submitted",
                    json!({
                        "organization": org_value,
                        "repository": repo_value,
                        "pull_request": { "node_id": pr_node_id },
                        "review": review,
                    }),
                )?);
            }
        }
    }

    Ok(changes)
}

fn required_node_id(value: &Value) -> Result<String> {
    value
        .get("node_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("GitHub entity missing 'node_id'"))
}

/// Build one synthetic webhook-envelope `action` field and hand the payload
/// to the shared `Converter`. Each call gets a fresh `effective_from`
/// timestamp (matching the streaming source's own per-delivery convention in
/// `drasi-source-github-workgraph`'s webhook ingress).
fn convert(
    source_id: &str,
    organization: &str,
    event_type: &str,
    action: &str,
    mut payload: Value,
) -> Result<Vec<SourceChange>> {
    if let Value::Object(map) = &mut payload {
        map.insert("action".to_string(), json!(action));
    }
    let effective_from = Utc::now().timestamp_millis().max(0) as u64;
    let converter = Converter::new(source_id, organization, effective_from);
    match converter.convert(event_type, &payload) {
        Ok(Some(changes)) => Ok(changes),
        Ok(None) => Ok(Vec::new()),
        Err(err) => Err(anyhow!(
            "GitHub WorkGraph mapping failed for '{event_type}'/'{action}': {err:?}"
        )),
    }
}

/// Normalize streaming changes into snapshot state. Existing elements become
/// inserts, while defensive deletes emitted by the shared converter (for
/// example, clearing a prior status error on an opened issue) do not represent
/// current graph state and must not be sent during bootstrap.
fn into_snapshot_insert(change: SourceChange) -> Option<SourceChange> {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            Some(SourceChange::Insert { element })
        }
        SourceChange::Delete { .. } | SourceChange::Future { .. } => None,
    }
}

fn source_change_label(change: &SourceChange) -> Option<Arc<str>> {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            element.get_metadata().labels.first().cloned()
        }
        SourceChange::Delete { metadata } => metadata.labels.first().cloned(),
        SourceChange::Future { .. } => None,
    }
}

/// Mirrors the `query_requests_any` convention used across other bootstrap
/// providers in this workspace (e.g. `cloudflare-radar`): when the request
/// carries no labels at all it means "send everything"; otherwise a change is
/// sent only if its own label is explicitly present in the matching
/// (node vs. relation) requested-labels list.
fn label_requested(request: &BootstrapRequest, label: &str, is_node: bool) -> bool {
    if request.node_labels.is_empty() && request.relation_labels.is_empty() {
        return true;
    }
    if is_node {
        request.node_labels.iter().any(|l| l == label)
    } else {
        request.relation_labels.iter().any(|l| l == label)
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_if_requested(
    change: SourceChange,
    request: &BootstrapRequest,
    source_id: &str,
    event_tx: &BootstrapEventSender,
    context: &BootstrapContext,
    sent_ids: &mut HashSet<String>,
) -> Result<usize> {
    let Some(change) = into_snapshot_insert(change) else {
        return Ok(0);
    };
    let Some(label) = source_change_label(&change) else {
        return Ok(0);
    };
    let is_node = NODE_LABELS.contains(&label.as_ref());
    let is_relation = RELATION_LABELS.contains(&label.as_ref());
    if !is_node && !is_relation {
        // Converter only ever emits the WorkGraph labels above; this branch
        // should be unreachable, but skip defensively rather than send an
        // unrecognized element type.
        return Ok(0);
    }
    if !label_requested(request, label.as_ref(), is_node) {
        return Ok(0);
    }
    let key = format!("{source_id}:{}", change.get_reference().element_id);
    if !sent_ids.insert(key) {
        return Ok(0);
    }
    let sequence = context.next_sequence();
    let event = BootstrapEvent {
        source_id: source_id.to_string(),
        change,
        timestamp: Utc::now(),
        sequence,
    };
    event_tx
        .send(event)
        .await
        .map_err(|err| anyhow!(err.to_string()))?;
    Ok(1)
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "github-workgraph-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::GitHubWorkGraphBootstrapDescriptor],
);
