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
//! Enumerates repositories in one configured GitHub organization over the
//! GitHub GraphQL v4 API, applies the Source's optional repository allowlist,
//! and snapshots open generic Issues and Pull Requests plus open and closed
//! configured WorkGraph task Issues, their parents, comments, and PR reviews.
//!
//! # Reuse, not duplication
//!
//! This crate never re-implements any WorkGraph domain rule. Every node
//! label, relation ID/direction, status-label derivation, and
//! `WorkGraphTask`/`WorkGraphTaskResult`/`WorkGraphError` parsing rule comes
//! from [`drasi_source_github_workgraph::mapping::Converter`] — the exact
//! same converter the streaming `drasi-source-github-workgraph` source uses
//! for live webhook deliveries. This crate's only job is to fetch GitHub data
//! over GraphQL and reshape it into the same JSON envelope shape a GitHub
//! webhook delivery would have (see [`client`] module docs), then hand that
//! envelope to `Converter` unchanged.
//!
//! The worker queue follows the same rule: the worker file is read with
//! [`drasi_source_github_workgraph::worker_client::WorkerFileClient`],
//! validated with [`drasi_source_github_workgraph::workers::parse_worker_file`],
//! and projected with
//! [`drasi_source_github_workgraph::mapping::worker_changes`] — the same three
//! pieces the live Source uses when a `push` touches the configured file.
//!
//! # Scope (prototype)
//!
//! - One configured organization; all repositories the token can see by
//!   default, or the Source's normalized repository allowlist.
//! - Open generic Issues and Pull Requests.
//! - Open and closed configured WorkGraph task Issues and all task comments.
//! - The configured worker-queue file, projected before every task artifact.
//! - Generic Issue/PR conversation comments and PR reviews.
//! - Excluded: Projects, Project Items, inline diff comments, closed-item
//!   non-task closed history, reactions, and workflow-run execution state.
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

pub use config::{GitHubWorkGraphBootstrapConfig, WorkerFileLocation};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use client::GitHubGraphQLClient;
use drasi_core::models::{Element, SourceChange};
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_source_github_workgraph::config::{LeaseTrust, RepositoryFilter, TaskIssueType};
use drasi_source_github_workgraph::lease_ledger::{LeaseLedger, LifecycleIntent};
use drasi_source_github_workgraph::mapping::Conversion;
use drasi_source_github_workgraph::mapping::{
    anchor_changes, worker_changes, Converter, WorkerProjection, NODE_LABELS, RELATION_LABELS,
};
use drasi_source_github_workgraph::worker_client::{WorkerFileClient, WorkerFileError};
use drasi_source_github_workgraph::workers::parse_worker_file;
use drasi_source_github_workgraph::workgraph::WorkGraphError;
use log::{info, warn};
use serde_json::{json, Value};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub struct GitHubWorkGraphBootstrapProvider {
    config: GitHubWorkGraphBootstrapConfig,
    repository_filter: RepositoryFilter,
}

impl GitHubWorkGraphBootstrapProvider {
    pub fn builder() -> GitHubWorkGraphBootstrapProviderBuilder {
        GitHubWorkGraphBootstrapProviderBuilder::new()
    }

    /// Fetch, validate, and project the configured worker file.
    ///
    /// The worker file is read with the same credential and endpoint as every
    /// other GitHub read here, and validated by exactly the same code the
    /// streaming Source uses, so the two can never disagree about a file.
    ///
    /// A file that cannot be *read* fails the bootstrap: nothing is known about
    /// configured capacity, and claiming an empty worker pool would silently
    /// stop every dispatch. A file that is read but deterministically
    /// *invalid* becomes an explicit `WorkGraphError` node instead, which is
    /// visible to queries and to an operator.
    async fn worker_changes(&self, source_id: &str) -> Result<Vec<SourceChange>> {
        let Some(location) = &self.config.worker_config else {
            return Ok(Vec::new());
        };
        let client = WorkerFileClient::new(&self.config.token, &self.config.api_base_url)
            .context("failed to build the GitHub worker file client")?;
        let effective_from = Utc::now().timestamp_millis().max(0) as u64;

        let rejected = |error: &WorkGraphError| {
            warn!(
                "[{source_id}] worker file at '{}' ref '{}' path '{}' rejected [{}]: {}",
                location.repository, location.r#ref, location.path, error.code, error.message
            );
            worker_changes(
                source_id,
                effective_from,
                location,
                &WorkerProjection::Rejected(error),
                &BTreeMap::new(),
                &BTreeMap::new(),
            )
        };

        let content = match client.fetch(location).await {
            Ok(content) => content,
            Err(WorkerFileError::Rejected(error)) => return Ok(rejected(&error)),
            Err(WorkerFileError::Unavailable(error)) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read the configured worker file '{}' at '{}' ref '{}'",
                        location.path, location.repository, location.r#ref
                    )
                })
            }
        };
        let file = match parse_worker_file(&content.text) {
            Ok(file) => file,
            Err(error) => return Ok(rejected(&error)),
        };
        info!(
            "[{source_id}] GitHub WorkGraph bootstrap: {} configured worker(s) from '{}' ref '{}' \
             path '{}'",
            file.workers.len(),
            location.repository,
            location.r#ref,
            location.path
        );
        Ok(worker_changes(
            source_id,
            effective_from,
            location,
            &WorkerProjection::Loaded {
                file: &file,
                content: &content,
            },
            // A bootstrap builds a fresh snapshot, so it has no prior
            // projection to retire slots against and nothing to remove.
            &BTreeMap::new(),
            &BTreeMap::new(),
        ))
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

    pub fn with_repositories(mut self, repositories: Vec<String>) -> Self {
        self.config.repositories = repositories;
        self
    }

    pub fn with_task_issue_type(mut self, task_issue_type: TaskIssueType) -> Self {
        self.config.task_issue_type = task_issue_type;
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

    pub fn with_worker_config(mut self, worker_config: WorkerFileLocation) -> Self {
        self.config.worker_config = Some(worker_config);
        self
    }

    pub fn with_lease_trust(mut self, lease_trust: LeaseTrust) -> Self {
        self.config.lease_trust = Some(lease_trust);
        self
    }

    pub fn build(self) -> Result<GitHubWorkGraphBootstrapProvider> {
        let config = self.config.normalized()?;
        let repository_filter = RepositoryFilter::new(&config.organization, &config.repositories)?;
        Ok(GitHubWorkGraphBootstrapProvider {
            config,
            repository_filter,
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

        // The configured worker file is fetched, validated, and projected
        // before any Issue or task artifact, so a query that bootstraps
        // capacity always sees the worker/slot graph ahead of the Assignments
        // and Leases that reference it.
        let mut all_changes: Vec<SourceChange> = self.worker_changes(&context.source_id).await?;

        let org_value = client.fetch_organization(&self.config.organization).await?;
        let repos = client.fetch_repositories(&self.config.organization).await?;
        let mut selected_repos = Vec::new();
        for repo in repos {
            if self
                .repository_filter
                .includes_repository(&repo)
                .context("GitHub repository cannot be matched against repositories filter")?
            {
                selected_repos.push(repo);
            }
        }
        info!(
            "[{}] GitHub WorkGraph bootstrap: {} selected repositories in '{}'",
            context.source_id,
            selected_repos.len(),
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
        for (index, repo) in selected_repos.into_iter().enumerate() {
            let client = client.clone();
            let organization = self.config.organization.clone();
            let source_id = context.source_id.clone();
            let org_value = org_value.clone();
            let semaphore = semaphore.clone();
            let task_issue_type = self.config.task_issue_type.clone();
            let repository_filter = self.repository_filter.clone();
            let lease_trust = self.config.lease_trust.clone();
            join_set.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow!("bootstrap concurrency semaphore closed"))?;
                let scope = ConversionScope {
                    organization: &organization,
                    task_issue_type: &task_issue_type,
                    repository_filter: &repository_filter,
                    lease_trust: lease_trust.as_ref(),
                };
                let changes =
                    process_repository(&client, &scope, &source_id, &org_value, repo).await?;
                Ok::<_, anyhow::Error>((index, changes))
            });
        }

        // Repository tasks finish in arbitrary order, so their results are
        // reassembled by the deterministic repository index before folding.
        // The projected snapshot must not depend on scheduling.
        type RepoOutput = (usize, (Vec<SourceChange>, Vec<LifecycleIntent>));
        let mut repo_changes: Vec<RepoOutput> = Vec::new();
        let mut repo_errors = Vec::new();
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok(Ok(changes)) => repo_changes.push(changes),
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
        repo_changes.sort_by_key(|(index, _)| *index);
        // Fold every task comment's current lease-lifecycle contribution, then
        // project each anchor from the artifacts that survive. This is the same
        // fold the live Source keeps in its durable ledger, so the same set of
        // current comments produces the same anchors either way.
        let mut ledger = LeaseLedger::new();
        for (_, (changes, lifecycle)) in repo_changes {
            all_changes.extend(changes);
            for intent in &lifecycle {
                ledger.apply(intent);
            }
        }
        let effective_from = Utc::now().timestamp_millis().max(0) as u64;
        all_changes.extend(anchor_changes(
            &context.source_id,
            effective_from,
            &ledger,
            ledger.anchor_ids(),
        ));

        // Folding, filtering, and event sending happen sequentially here
        // (after all concurrent fetches have completed) so `event_count` and
        // the snapshot fold below stay simple and deterministic without a
        // shared, lock-protected accumulator across tasks.
        let mut event_count = 0usize;
        for change in fold_snapshot(all_changes) {
            event_count +=
                send_if_requested(change, &request, &context.source_id, &event_tx, context).await?;
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
/// The Source-owned scope every conversion needs: which organization, task
/// Issue Type, repositories, and lease-lifecycle producers are in play.
struct ConversionScope<'a> {
    organization: &'a str,
    task_issue_type: &'a TaskIssueType,
    repository_filter: &'a RepositoryFilter,
    lease_trust: Option<&'a LeaseTrust>,
}

async fn process_repository(
    client: &GitHubGraphQLClient,
    scope: &ConversionScope<'_>,
    source_id: &str,
    org_value: &Value,
    repo_value: Value,
) -> Result<(Vec<SourceChange>, Vec<LifecycleIntent>)> {
    let (task_issue_type, repository_filter) = (scope.task_issue_type, scope.repository_filter);
    let mut lifecycle = Vec::new();
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

    changes.extend(collect(
        &mut lifecycle,
        convert(
            source_id,
            scope,
            "repository",
            "created",
            json!({ "organization": org_value, "repository": repo_value }),
        )?,
    ));

    let issues = client
        .fetch_issues(&owner, &name)
        .await
        .with_context(|| format!("failed to fetch issues for {owner}/{name}"))?;
    for issue in issues {
        if task_issue_type.matches(issue.get("type")) {
            continue;
        }
        let issue_node_id = required_node_id(&issue)?;
        changes.extend(collect(
            &mut lifecycle,
            convert(
                source_id,
                scope,
                "issues",
                "opened",
                json!({
                    "organization": org_value,
                    "repository": repo_value,
                    "issue": issue,
                }),
            )?,
        ));

        if client::item_comment_count(&issue) > 0 {
            let comments = client
                .fetch_issue_comments(&issue_node_id)
                .await
                .with_context(|| {
                    format!("failed to fetch comments for issue {owner}/{name}#{issue_node_id}")
                })?;
            for comment in comments {
                changes.extend(collect(
                    &mut lifecycle,
                    convert(
                        source_id,
                        scope,
                        "issue_comment",
                        "created",
                        json!({
                            "organization": org_value,
                            "repository": repo_value,
                            "issue": { "node_id": issue_node_id, "state": "open" },
                            "comment": comment,
                        }),
                    )?,
                ));
            }
        }
    }

    let tasks = client
        .fetch_tasks(&owner, &name, task_issue_type)
        .await
        .with_context(|| format!("failed to fetch WorkGraph tasks for {owner}/{name}"))?;
    for task in tasks {
        let task_node_id = required_node_id(&task)?;
        let action = if task.get("state").and_then(Value::as_str) == Some("closed") {
            "closed"
        } else {
            "opened"
        };
        changes.extend(collect(
            &mut lifecycle,
            convert(
                source_id,
                scope,
                "issues",
                action,
                json!({
                    "organization": org_value,
                    "repository": repo_value,
                    "issue": task,
                }),
            )?,
        ));

        if let Some(parent) = task.get("parent").filter(|parent| !parent.is_null()) {
            let parent_repo = parent
                .get("repository")
                .ok_or_else(|| anyhow!("task parent missing repository"))?;
            if repository_filter
                .includes_repository(parent_repo)
                .context("task parent repository cannot be matched against repositories filter")?
            {
                changes.extend(collect(
                    &mut lifecycle,
                    convert(
                        source_id,
                        scope,
                        "repository",
                        "created",
                        json!({
                            "organization": org_value,
                            "repository": parent_repo,
                        }),
                    )?,
                ));
            }
            changes.extend(collect(
                &mut lifecycle,
                convert(
                    source_id,
                    scope,
                    "sub_issues",
                    "sub_issue_added",
                    json!({
                        "organization": org_value,
                        "repository": parent_repo,
                        "parent_issue": parent,
                        "parent_issue_repo": parent_repo,
                        "sub_issue": task,
                        "sub_issue_repo": repo_value,
                    }),
                )?,
            ));
        }

        if client::item_comment_count(&task) > 0 {
            let comments = client
                .fetch_issue_comments(&task_node_id)
                .await
                .with_context(|| {
                    format!("failed to fetch comments for task {owner}/{name}#{task_node_id}")
                })?;
            for comment in comments {
                changes.extend(collect(
                    &mut lifecycle,
                    convert(
                        source_id,
                        scope,
                        "issue_comment",
                        "created",
                        json!({
                            "organization": org_value,
                            "repository": repo_value,
                            "issue": task,
                            "comment": comment,
                        }),
                    )?,
                ));
            }
        }
    }

    let pull_requests = client
        .fetch_pull_requests(&owner, &name)
        .await
        .with_context(|| format!("failed to fetch pull requests for {owner}/{name}"))?;
    for pr in pull_requests {
        let pr_node_id = required_node_id(&pr)?;
        changes.extend(collect(
            &mut lifecycle,
            convert(
                source_id,
                scope,
                "pull_request",
                "opened",
                json!({
                    "organization": org_value,
                    "repository": repo_value,
                    "pull_request": pr,
                }),
            )?,
        ));

        if client::item_comment_count(&pr) > 0 {
            let comments = client
                .fetch_pr_comments(&pr_node_id)
                .await
                .with_context(|| {
                    format!("failed to fetch comments for pull request {owner}/{name}#{pr_node_id}")
                })?;
            for comment in comments {
                changes.extend(collect(
                    &mut lifecycle,
                    convert(
                        source_id,
                        scope,
                        "issue_comment",
                        "created",
                        json!({
                            "organization": org_value,
                            "repository": repo_value,
                            // `pull_request` present (even empty) is exactly the
                            // discriminator `mapping::comment_event` checks via
                            // `issue.get("pull_request").is_some()`.
                            "issue": {
                                "node_id": pr_node_id,
                                "state": "open",
                                "pull_request": {}
                            },
                            "comment": comment,
                        }),
                    )?,
                ));
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
                changes.extend(collect(
                    &mut lifecycle,
                    convert(
                        source_id,
                        scope,
                        "pull_request_review",
                        "submitted",
                        json!({
                            "organization": org_value,
                            "repository": repo_value,
                            "pull_request": { "node_id": pr_node_id, "state": "open" },
                            "review": review,
                        }),
                    )?,
                ));
            }
        }
    }

    Ok((changes, lifecycle))
}

/// Split a conversion, accumulating its lease-lifecycle contributions.
fn collect(lifecycle: &mut Vec<LifecycleIntent>, conversion: Conversion) -> Vec<SourceChange> {
    lifecycle.extend(conversion.lifecycle);
    conversion.changes
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
    scope: &ConversionScope<'_>,
    event_type: &str,
    action: &str,
    mut payload: Value,
) -> Result<Conversion> {
    if let Value::Object(map) = &mut payload {
        map.insert("action".to_string(), json!(action));
    }
    let effective_from = Utc::now().timestamp_millis().max(0) as u64;
    let converter = Converter::new(
        source_id,
        scope.organization,
        scope.task_issue_type,
        effective_from,
    )
    .with_repository_filter(scope.repository_filter);
    let converter = match scope.lease_trust {
        Some(lease_trust) => converter.with_lease_trust(lease_trust),
        None => converter,
    };
    match converter.convert(event_type, &payload) {
        Ok(Some(conversion)) => Ok(conversion),
        Ok(None) => Ok(Conversion {
            changes: Vec::new(),
            lifecycle: Vec::new(),
            lifecycle_scope: None,
            lifecycle_anchors: Vec::new(),
        }),
        Err(err) => Err(anyhow!(
            "GitHub WorkGraph mapping failed for '{event_type}'/'{action}': {err:?}"
        )),
    }
}

/// Fold the converted change stream into snapshot state.
///
/// The streaming converter emits an ordered stream of creates, convergent
/// updates, and defensive deletes. A snapshot is that stream's *final* state,
/// so it is replayed here rather than approximated:
///
/// * a repeated element converges — a later `Update` merges over the earlier
///   element exactly as [`drasi_core`] merges it at query time, so an entity
///   observed more than once (a repository reached both directly and as a task
///   parent, for example) lands in the same state the live Source reaches;
/// * a `Delete` removes an element the stream had already produced, and is a
///   no-op for an element that was never produced (the common case for the
///   converter's defensive deletes);
/// * first-appearance order is preserved so the emitted snapshot is stable.
fn fold_snapshot(changes: Vec<SourceChange>) -> Vec<SourceChange> {
    let mut order: Vec<Arc<str>> = Vec::new();
    let mut elements: HashMap<Arc<str>, Element> = HashMap::new();
    for change in changes {
        match change {
            SourceChange::Insert { element } | SourceChange::Update { element } => {
                let id = element.get_reference().element_id.clone();
                match elements.entry(id.clone()) {
                    Entry::Occupied(mut occupied) => {
                        let mut merged = element;
                        // Node and relation IDs are namespaced by construction,
                        // so a type change cannot happen; skip rather than
                        // panic if one ever does.
                        if std::mem::discriminant(&merged) == std::mem::discriminant(occupied.get())
                        {
                            merged.merge_missing_properties(occupied.get());
                        }
                        occupied.insert(merged);
                    }
                    Entry::Vacant(vacant) => {
                        order.push(id);
                        vacant.insert(element);
                    }
                }
            }
            SourceChange::Delete { metadata } => {
                let id = &metadata.reference.element_id;
                if elements.remove(id).is_some() {
                    order.retain(|entry| entry != id);
                }
            }
            SourceChange::Future { .. } => {}
        }
    }
    order
        .into_iter()
        .filter_map(|id| elements.remove(&id))
        .map(|element| SourceChange::Insert { element })
        .collect()
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
) -> Result<usize> {
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
