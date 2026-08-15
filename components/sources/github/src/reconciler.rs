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

//! Periodic reconciler for missed changes and project-derived repo scope updates.

use crate::config::ProjectSpec;
use crate::graphql::GitHubGraphQLClient;
use crate::hydrator::{
    load_reconcile_state, prepare_reconcile_transition, replay_pending_delta, save_effective_repos,
    save_reconcile_state,
};
use crate::mapping::{map_reconcile_snapshot, repositories_from_project_items};
use anyhow::{Context, Result};
use drasi_lib::sources::base::SourceBase;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::WalProvider;
use log::{debug, info, warn};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Duration;

pub struct ReconcilerParams {
    pub source_id: String,
    pub base: SourceBase,
    pub state_store: Arc<dyn StateStoreProvider>,
    pub wal: Arc<dyn WalProvider>,
    pub api_client: Arc<GitHubGraphQLClient>,
    pub projects: Vec<ProjectSpec>,
    pub static_repos: HashSet<String>,
    pub effective_repos: Arc<RwLock<HashSet<String>>>,
    pub interval_secs: u64,
    pub run_initial_pass: bool,
    pub processing_gate: Arc<Mutex<()>>,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

pub async fn run_reconciler_loop(params: ReconcilerParams) -> Result<()> {
    info!("[{}] Reconciler loop started", params.source_id);
    let mut shutdown = params.shutdown.clone();

    if params.run_initial_pass {
        if let Err(err) = reconcile_once(&params).await {
            warn!(
                "[{}] Initial reconcile pass failed: {:#}",
                params.source_id, err
            );
        }
    }

    let mut interval = tokio::time::interval(Duration::from_secs(params.interval_secs));
    // Skip the immediate first tick so the interval is truly periodic.
    interval.tick().await;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(err) = reconcile_once(&params).await {
                    warn!("[{}] Reconcile pass failed: {:#}", params.source_id, err);
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    info!("[{}] Reconciler loop stopped", params.source_id);
    Ok(())
}

pub(crate) async fn reconcile_once(params: &ReconcilerParams) -> Result<()> {
    let _processing_guard = params.processing_gate.lock().await;
    replay_pending_delta(params.state_store.as_ref(), &params.source_id, &params.base).await?;
    let wal_coverage_sequence = params
        .wal
        .head_sequence(&params.source_id)
        .await
        .context("Failed reading WAL head for reconcile coverage")?;

    let mut dynamic_project_repos = HashSet::new();
    for project in &params.projects {
        let items = params
            .api_client
            .fetch_project_items(project)
            .await
            .with_context(|| {
                format!(
                    "Failed fetching project items for owner={} number={}",
                    project.owner, project.number
                )
            })?;
        dynamic_project_repos.extend(repositories_from_project_items(&items));
    }

    let mut next_effective = params.static_repos.clone();
    next_effective.extend(dynamic_project_repos);
    next_effective = next_effective
        .into_iter()
        .map(|r| r.to_ascii_lowercase())
        .collect();

    {
        let mut guard = params.effective_repos.write().await;
        *guard = next_effective.clone();
    }
    save_effective_repos(
        params.state_store.as_ref(),
        &params.source_id,
        &next_effective,
    )
    .await?;

    let repos_for_fetch = next_effective.iter().cloned().collect::<Vec<_>>();
    debug!(
        "[{}] Reconcile effective repos: {:?}",
        params.source_id, repos_for_fetch
    );
    let snapshot = params
        .api_client
        .fetch_reconcile_snapshot(&repos_for_fetch, &params.projects)
        .await
        .context("Failed fetching full reconcile snapshot")?;

    let mut reconcile_state =
        load_reconcile_state(params.state_store.as_ref(), &params.source_id).await?;
    let previous_index = reconcile_state.index.clone();
    let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let (changes, next_index) = map_reconcile_snapshot(
        &params.source_id,
        &snapshot,
        &previous_index,
        effective_from,
    );

    prepare_reconcile_transition(
        &mut reconcile_state,
        next_index,
        changes,
        wal_coverage_sequence,
    );
    save_reconcile_state(
        params.state_store.as_ref(),
        &params.source_id,
        &reconcile_state,
    )
    .await?;
    replay_pending_delta(params.state_store.as_ref(), &params.source_id, &params.base).await?;
    Ok(())
}
