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

//! Strictly-sequential hydrator for inbox admissions.

use crate::config::ProjectSpec;
use crate::graphql::{FetchedRoot, GitHubGraphQLClient, ProjectItemContent, RetryableGraphQLError};
use crate::mapping::{map_root_current, map_webhook_object_delete, CurrentChangeKind};
use crate::types::{HydratorHealth, WebhookLocator};
use crate::webhook::{decode_admission_change, warn_unhealthy_hydrator};
use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
use drasi_lib::sources::base::SourceBase;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::WalProvider;
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::{sleep, Duration};

const EFFECTIVE_REPOS_KEY: &str = "effective-repos";
const NULL_RETRY_PREFIX: &str = "null-retry:";
pub(crate) const MAX_NULL_HYDRATION_ATTEMPTS: u32 = 3;

#[derive(Debug)]
struct RetryableHydrationError(anyhow::Error);

impl fmt::Display for RetryableHydrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for RetryableHydrationError {}

pub struct HydratorParams {
    pub source_id: String,
    pub inbox_source_id: String,
    pub output_source_id: String,
    pub base: SourceBase,
    pub wal: Arc<dyn WalProvider>,
    pub state_store: Arc<dyn StateStoreProvider>,
    pub api_client: Arc<GitHubGraphQLClient>,
    pub projects: Vec<ProjectSpec>,
    pub effective_repos: Arc<RwLock<HashSet<String>>>,
    pub output_gate: Arc<Mutex<()>>,
    pub notify: Arc<Notify>,
    pub health: Arc<RwLock<HydratorHealth>>,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

pub async fn run_hydrator_loop(params: HydratorParams) -> Result<()> {
    info!("[{}] Hydrator loop started", params.source_id);
    let mut shutdown = params.shutdown.clone();
    let mut retry_count = 0u32;

    loop {
        if *shutdown.borrow() {
            break;
        }

        let maybe_oldest = params
            .wal
            .oldest_sequence(&params.inbox_source_id)
            .await
            .context("Inbox WAL oldest_sequence failed")?;

        let Some(oldest) = maybe_oldest else {
            tokio::select! {
                _ = params.notify.notified() => {},
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
            continue;
        };

        let entries = params
            .wal
            .read_from(&params.inbox_source_id, oldest)
            .await
            .context("Inbox WAL read_from failed")?;

        let Some((sequence, admission)) = entries.into_iter().next() else {
            params.notify.notified().await;
            continue;
        };

        match process_admission(&params, sequence, &admission).await {
            Ok(()) => {
                retry_count = 0;
                *params.health.write().await = HydratorHealth::default();
            }
            Err(err) => {
                if err.downcast_ref::<RetryableHydrationError>().is_none() {
                    return Err(err.context("Hydrator encountered a terminal processing failure"));
                }
                retry_count = retry_count.saturating_add(1);
                let delay_secs = (1u64 << retry_count.min(6)).min(60);
                let delivery_id = decode_admission_change(&admission)
                    .map(|(id, _)| id)
                    .unwrap_or_else(|_| "unknown".to_string());

                {
                    let mut health = params.health.write().await;
                    health.stalled_delivery_id = if delivery_id.is_empty() {
                        None
                    } else {
                        Some(delivery_id)
                    };
                    health.retry_count = retry_count;
                    health.next_retry_secs = Some(delay_secs);
                    health.last_error = Some(format!("{err:#}"));
                    warn_unhealthy_hydrator(&params.source_id, &health);
                }

                warn!(
                    "[{}] Hydrator admission processing failed (retry_count={}, next={}s): {:#}",
                    params.source_id, retry_count, delay_secs, err
                );
                sleep(Duration::from_secs(delay_secs)).await;
            }
        }
    }

    info!("[{}] Hydrator loop stopped", params.source_id);
    Ok(())
}

pub(crate) async fn process_admission(
    params: &HydratorParams,
    sequence: u64,
    admission: &drasi_core::models::SourceChange,
) -> Result<()> {
    let (delivery_id, locator) = decode_admission_change(admission)
        .with_context(|| format!("Failed to decode inbox admission at sequence {sequence}"))?;
    debug!(
        "[{}] Processing delivery {} seq={} event={} action={} node_id={:?}",
        params.source_id,
        delivery_id,
        sequence,
        locator.event_type,
        locator.action,
        locator.node_id
    );

    if !is_supported_event_type(&locator.event_type) {
        prune_inbox(params, sequence).await?;
        clear_null_retry(params.state_store.as_ref(), &params.source_id, &delivery_id).await?;
        return Ok(());
    }

    if locator.action == "deleted" {
        let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let change = map_webhook_object_delete(&params.source_id, &locator, effective_from)?;
        emit_output_changes(params, std::slice::from_ref(&change)).await?;
        clear_null_retry(params.state_store.as_ref(), &params.source_id, &delivery_id).await?;
        prune_inbox(params, sequence).await?;
        return Ok(());
    }

    let fetched = params
        .api_client
        .fetch_root_from_locator(&locator)
        .await
        .map_err(|err| {
            if err.downcast_ref::<RetryableGraphQLError>().is_some() {
                RetryableHydrationError(err).into()
            } else {
                err
            }
        })
        .with_context(|| {
            format!(
                "Failed to hydrate locator event={} action={} node_id={:?}",
                locator.event_type, locator.action, locator.node_id
            )
        })?;

    if fetched.is_none() && !is_authoritative_absence_action(&locator) {
        let attempts =
            increment_null_retry(params.state_store.as_ref(), &params.source_id, &delivery_id)
                .await?;
        if attempts >= MAX_NULL_HYDRATION_ATTEMPTS {
            warn!(
                "[{}] Delivery {} reached null-hydration retry bound ({MAX_NULL_HYDRATION_ATTEMPTS}); advancing FIFO",
                params.source_id, delivery_id
            );
            clear_null_retry(params.state_store.as_ref(), &params.source_id, &delivery_id).await?;
            prune_inbox(params, sequence).await?;
            return Ok(());
        }
        return Err(RetryableHydrationError(anyhow::anyhow!(
            "GraphQL returned node=null for non-delete action '{}' (event '{}'); transient attempt {attempts}/{MAX_NULL_HYDRATION_ATTEMPTS}",
            locator.action,
            locator.event_type
        ))
        .into());
    }
    clear_null_retry(params.state_store.as_ref(), &params.source_id, &delivery_id).await?;

    let Some(fetched) = fetched else {
        prune_inbox(params, sequence).await?;
        return Ok(());
    };

    if let Some(identity) = project_identity(&locator, Some(&fetched))? {
        if !is_project_configured(&params.projects, &identity.owner, identity.number) {
            debug!(
                "[{}] Skipping delivery {} for unconfigured project owner={} number={}",
                params.source_id, delivery_id, identity.owner, identity.number
            );
            prune_inbox(params, sequence).await?;
            return Ok(());
        }
    } else if is_project_event(&locator) {
        debug!(
            "[{}] Skipping project delivery {} because project identity could not be resolved",
            params.source_id, delivery_id
        );
        prune_inbox(params, sequence).await?;
        return Ok(());
    }

    if let Some(repo) = fetched
        .repository_full_name()
        .map(|r| r.to_ascii_lowercase())
    {
        if !is_repo_effective(&params.effective_repos, &repo).await {
            debug!(
                "[{}] Skipping delivery {} because authoritative repository {} is outside scope",
                params.source_id, delivery_id, repo
            );
            prune_inbox(params, sequence).await?;
            return Ok(());
        }
    } else if let Some(repo) = locator.repository_full_name.as_ref() {
        let repo = repo.to_ascii_lowercase();
        if !is_project_event(&locator) && !is_repo_effective(&params.effective_repos, &repo).await {
            debug!(
                "[{}] Skipping delivery {} for non-scoped repository {}",
                params.source_id, delivery_id, repo
            );
            prune_inbox(params, sequence).await?;
            return Ok(());
        }
    }

    if let FetchedRoot::ProjectItem(item) = &fetched {
        if let Some(repo_name) = project_item_repo(item) {
            add_effective_repo(
                params.state_store.as_ref(),
                &params.source_id,
                &params.effective_repos,
                &repo_name,
            )
            .await?;
        }
    }

    let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let change_kind = if is_creation_action(&locator.action) {
        CurrentChangeKind::Insert
    } else {
        CurrentChangeKind::Update
    };
    let changes = map_root_current(&params.source_id, &fetched, change_kind, effective_from);
    emit_output_changes(params, &changes).await?;

    prune_inbox(params, sequence).await?;
    Ok(())
}

async fn emit_output_changes(
    params: &HydratorParams,
    changes: &[drasi_core::models::SourceChange],
) -> Result<()> {
    for change in changes {
        let _output_guard = params.output_gate.lock().await;
        let output_seq = params
            .wal
            .append(&params.output_source_id, change)
            .await
            .context("Failed appending normalized change to output WAL")?;
        let mut wrapper = SourceEventWrapper::new(
            params.source_id.clone(),
            SourceEvent::Change(change.clone()),
            chrono::Utc::now(),
        );
        wrapper.sequence = Some(output_seq);
        wrapper.source_position = Some(Bytes::from(output_seq.to_be_bytes().to_vec()));
        if let Err(err) = params.base.dispatch_event(wrapper).await {
            warn!(
                "[{}] Live dispatch failed after durable output append (seq={}): {:#}",
                params.source_id, output_seq, err
            );
        }
    }
    Ok(())
}

async fn prune_inbox(params: &HydratorParams, sequence: u64) -> Result<()> {
    params
        .wal
        .prune_up_to(&params.inbox_source_id, sequence)
        .await
        .with_context(|| format!("Failed pruning inbox WAL through sequence {sequence}"))?;
    Ok(())
}

async fn is_repo_effective(effective_repos: &Arc<RwLock<HashSet<String>>>, repo: &str) -> bool {
    effective_repos
        .read()
        .await
        .contains(&repo.to_ascii_lowercase())
}

fn is_supported_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "repository"
            | "issues"
            | "pull_request"
            | "issue_comment"
            | "pull_request_review"
            | "pull_request_review_comment"
            | "projects_v2"
            | "projects_v2_item"
    )
}

fn is_project_event(locator: &WebhookLocator) -> bool {
    matches!(
        locator.event_type.as_str(),
        "projects_v2" | "projects_v2_item"
    )
}

fn is_authoritative_absence_action(locator: &WebhookLocator) -> bool {
    let action = locator.action.as_str();
    match locator.event_type.as_str() {
        "issues" => matches!(action, "transferred"),
        "pull_request_review" => matches!(action, "dismissed"),
        "projects_v2_item" | "project_item" => matches!(action, "archived"),
        "repository" => matches!(action, "archived"),
        _ => false,
    }
}

fn is_creation_action(action: &str) -> bool {
    matches!(action, "created" | "opened" | "submitted")
}

fn is_project_configured(projects: &[ProjectSpec], owner: &str, number: u32) -> bool {
    projects
        .iter()
        .any(|project| project.owner.eq_ignore_ascii_case(owner) && project.number == number)
}

#[derive(Debug, Clone)]
struct ProjectIdentity {
    owner: String,
    number: u32,
}

fn project_identity(
    locator: &WebhookLocator,
    fetched: Option<&FetchedRoot>,
) -> Result<Option<ProjectIdentity>> {
    if let Some(root) = fetched {
        match root {
            FetchedRoot::Project(project) => {
                let number =
                    u32::try_from(project.number).context("Project number does not fit in u32")?;
                return Ok(Some(ProjectIdentity {
                    owner: project.owner.login.clone(),
                    number,
                }));
            }
            FetchedRoot::ProjectItem(item) => {
                let number = u32::try_from(item.project.number)
                    .context("Project item parent number does not fit in u32")?;
                return Ok(Some(ProjectIdentity {
                    owner: item.project.owner.login.clone(),
                    number,
                }));
            }
            _ => {}
        }
    }

    if let (Some(owner), Some(number)) = (locator.project_owner.as_ref(), locator.project_number) {
        return Ok(Some(ProjectIdentity {
            owner: owner.clone(),
            number,
        }));
    }
    Ok(None)
}

fn project_item_repo(item: &crate::graphql::ProjectItemData) -> Option<String> {
    let content = item.content.as_ref()?;
    match content {
        ProjectItemContent::Issue { repository, .. }
        | ProjectItemContent::PullRequest { repository, .. } => {
            Some(repository.name_with_owner.to_ascii_lowercase())
        }
        ProjectItemContent::DraftIssue { .. } => None,
    }
}

async fn increment_null_retry(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    delivery_id: &str,
) -> Result<u32> {
    let key = format!("{NULL_RETRY_PREFIX}{delivery_id}");
    let current = state_store
        .get(source_id, &key)
        .await
        .with_context(|| format!("state_store.get({key}) failed"))?
        .map(|data| serde_json::from_slice::<u32>(&data))
        .transpose()
        .with_context(|| format!("Failed to parse retry counter for key {key}"))?
        .unwrap_or(0);
    let next = current.saturating_add(1);
    state_store
        .set(
            source_id,
            &key,
            serde_json::to_vec(&next).context("Failed to serialize retry counter")?,
        )
        .await
        .with_context(|| format!("state_store.set({key}) failed"))?;
    Ok(next)
}

async fn clear_null_retry(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    delivery_id: &str,
) -> Result<()> {
    let key = format!("{NULL_RETRY_PREFIX}{delivery_id}");
    state_store
        .delete(source_id, &key)
        .await
        .with_context(|| format!("state_store.delete({key}) failed"))?;
    Ok(())
}

pub async fn load_effective_repos(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
) -> Result<HashSet<String>> {
    let bytes = state_store
        .get(source_id, EFFECTIVE_REPOS_KEY)
        .await
        .map_err(|e| anyhow::anyhow!("state_store.get({EFFECTIVE_REPOS_KEY}) failed: {e}"))?;
    match bytes {
        Some(data) => Ok(serde_json::from_slice::<HashSet<String>>(&data)
            .context("Failed to parse effective repos set")?),
        None => Ok(HashSet::new()),
    }
}

pub async fn save_effective_repos(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    repos: &HashSet<String>,
) -> Result<()> {
    let payload = serde_json::to_vec(repos).context("Failed to serialize effective repos")?;
    state_store
        .set(source_id, EFFECTIVE_REPOS_KEY, payload)
        .await
        .map_err(|e| anyhow::anyhow!("state_store.set({EFFECTIVE_REPOS_KEY}) failed: {e}"))
}

pub async fn add_effective_repo(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    effective_repos: &Arc<RwLock<HashSet<String>>>,
    repo: &str,
) -> Result<bool> {
    let mut guard = effective_repos.write().await;
    let inserted = guard.insert(repo.to_ascii_lowercase());
    if inserted {
        save_effective_repos(state_store, source_id, &guard).await?;
    }
    Ok(inserted)
}
