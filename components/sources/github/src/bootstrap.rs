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

//! Bootstrap provider for initial GitHub snapshot loading.

use crate::config::GitHubSourceConfig;
use crate::graphql::{FetchedRoot, GitHubGraphQLClient, ReconcileSnapshot};
use crate::hydrator::{
    load_reconcile_state, prepare_reconcile_transition, replay_pending_delta, save_effective_repos,
    save_reconcile_state, save_root_snapshot,
};
use crate::mapping::{map_reconcile_snapshot, map_root_diff, repositories_from_project_items};
use crate::types::RootSnapshot;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_lib::sources::base::SourceBase;
use drasi_lib::state_store::StateStoreProvider;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, RwLock};

pub struct GitHubBootstrapProvider {
    config: GitHubSourceConfig,
    effective_repos: Arc<RwLock<HashSet<String>>>,
    processing_gate: Arc<Mutex<()>>,
    source_base: Arc<OnceLock<SourceBase>>,
}

impl GitHubBootstrapProvider {
    pub fn new(
        config: GitHubSourceConfig,
        effective_repos: Arc<RwLock<HashSet<String>>>,
        processing_gate: Arc<Mutex<()>>,
        source_base: Arc<OnceLock<SourceBase>>,
    ) -> Self {
        Self {
            config,
            effective_repos,
            processing_gate,
            source_base,
        }
    }
}

#[async_trait]
impl BootstrapProvider for GitHubBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let _processing_guard = self.processing_gate.lock().await;
        let source_base = self
            .source_base
            .get()
            .ok_or_else(|| anyhow!("GitHub bootstrap source dispatcher is not initialized"))?;
        let state_store = self
            .source_base
            .get()
            .expect("source base checked above")
            .state_store()
            .await
            .ok_or_else(|| anyhow!("GitHub bootstrap requires an initialized state store"))?;
        if !state_store.is_durable() {
            return Err(anyhow!(
                "GitHub bootstrap requires a durable state store provider (is_durable=true)"
            ));
        }
        replay_pending_delta(state_store.as_ref(), &context.source_id, source_base).await?;

        let wal_coverage_sequence = match source_base
            .context()
            .await
            .and_then(|runtime| runtime.wal_provider)
        {
            Some(wal) => wal.head_sequence(&context.source_id).await.unwrap_or(0),
            None => 0,
        };

        let client =
            GitHubGraphQLClient::new(self.config.graphql_url.clone(), self.config.token.clone())
                .context("Failed to create GitHub GraphQL client for bootstrap")?;

        let mut effective_repos = self.config.static_repository_set()?;
        for project in &self.config.projects {
            let project_items = client.fetch_project_items(project).await.with_context(|| {
                format!(
                    "Failed to fetch project items for {}#{}",
                    project.owner, project.number
                )
            })?;
            effective_repos.extend(repositories_from_project_items(&project_items));
        }

        let repos_vec = effective_repos.iter().cloned().collect::<Vec<_>>();
        let snapshot = client
            .fetch_reconcile_snapshot(&repos_vec, &self.config.projects)
            .await
            .context("Failed to fetch bootstrap snapshot")?;

        let mut reconcile_state =
            load_reconcile_state(state_store.as_ref(), &context.source_id).await?;
        let previous_index = reconcile_state.index.clone();
        let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let (full_snapshot_changes, _) = map_reconcile_snapshot(
            &context.source_id,
            &snapshot,
            &HashMap::new(),
            effective_from,
        );
        let (delta, next_index) = map_reconcile_snapshot(
            &context.source_id,
            &snapshot,
            &previous_index,
            effective_from,
        );
        let root_snapshots = map_snapshot_roots(
            &context.source_id,
            &snapshot,
            &previous_index,
            &next_index,
            effective_from,
        )?;

        save_snapshot_roots(state_store.as_ref(), &context.source_id, root_snapshots).await?;
        save_effective_repos(state_store.as_ref(), &context.source_id, &effective_repos).await?;
        prepare_reconcile_transition(
            &mut reconcile_state,
            next_index,
            delta,
            wal_coverage_sequence,
        );
        save_reconcile_state(state_store.as_ref(), &context.source_id, &reconcile_state).await?;
        *self.effective_repos.write().await = effective_repos;
        replay_pending_delta(state_store.as_ref(), &context.source_id, source_base).await?;

        let node_filter: HashSet<String> = request.node_labels.into_iter().collect();
        let rel_filter: HashSet<String> = request.relation_labels.into_iter().collect();

        let mut sent = 0usize;
        for change in full_snapshot_changes {
            if !label_matches(&change, &node_filter, &rel_filter) {
                continue;
            }

            let event = BootstrapEvent {
                source_id: context.source_id.clone(),
                change,
                timestamp: chrono::Utc::now(),
                sequence: context.next_sequence(),
            };
            event_tx
                .send(event)
                .await
                .context("Bootstrap receiver closed before the full snapshot was emitted")?;
            sent += 1;
        }

        source_base
            .mark_bootstrap_query_ready(&request.query_id)
            .await;
        replay_pending_delta(state_store.as_ref(), &context.source_id, source_base).await?;

        Ok(BootstrapResult {
            event_count: sent,
            source_position: None,
        })
    }
}

fn map_snapshot_roots(
    source_id: &str,
    snapshot: &ReconcileSnapshot,
    previous_index: &HashMap<String, crate::types::SnapshotElement>,
    next_index: &HashMap<String, crate::types::SnapshotElement>,
    effective_from: u64,
) -> Result<Vec<(String, RootSnapshot)>> {
    let roots = snapshot
        .repositories
        .values()
        .cloned()
        .map(FetchedRoot::Repository)
        .chain(snapshot.issues.values().cloned().map(FetchedRoot::Issue))
        .chain(
            snapshot
                .pull_requests
                .values()
                .cloned()
                .map(FetchedRoot::PullRequest),
        )
        .chain(
            snapshot
                .issue_comments
                .values()
                .cloned()
                .map(FetchedRoot::IssueComment),
        )
        .chain(
            snapshot
                .reviews
                .values()
                .cloned()
                .map(FetchedRoot::PullRequestReview),
        )
        .chain(
            snapshot
                .review_comments
                .values()
                .cloned()
                .map(FetchedRoot::PullRequestReviewComment),
        )
        .chain(
            snapshot
                .projects
                .values()
                .cloned()
                .map(FetchedRoot::Project),
        )
        .chain(
            snapshot
                .project_items
                .values()
                .cloned()
                .map(FetchedRoot::ProjectItem),
        );

    let mut snapshots = Vec::new();
    for root in roots {
        let (_, root_snapshot) = map_root_diff(source_id, &root, None, effective_from)?;
        snapshots.push((root.root_id().to_string(), root_snapshot));
    }

    for (id, previous) in previous_index {
        if previous.element_type != "node" || next_index.contains_key(id) {
            continue;
        }
        snapshots.push((
            id.clone(),
            RootSnapshot {
                root_id: id.clone(),
                root_kind: previous.labels.first().cloned().unwrap_or_default(),
                repository_full_name: None,
                committed_delivery_id: None,
                committed_sequence: None,
                elements: HashMap::new(),
            },
        ));
    }
    Ok(snapshots)
}

async fn save_snapshot_roots(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    snapshots: Vec<(String, RootSnapshot)>,
) -> Result<()> {
    for (root_id, root_snapshot) in snapshots {
        save_root_snapshot(
            state_store,
            source_id,
            &format!("root-snapshot:{root_id}"),
            &root_snapshot,
        )
        .await?;
    }
    Ok(())
}

fn label_matches(
    change: &SourceChange,
    node_filter: &HashSet<String>,
    rel_filter: &HashSet<String>,
) -> bool {
    if node_filter.is_empty() && rel_filter.is_empty() {
        return true;
    }

    let labels = match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            element.get_metadata().labels.clone()
        }
        SourceChange::Delete { metadata } => metadata.labels.clone(),
        SourceChange::Future { .. } => return false,
    };

    for label in labels.iter() {
        let label = label.as_ref();
        if node_filter.contains(label) || rel_filter.contains(label) {
            return true;
        }
    }
    false
}
