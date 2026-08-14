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

//! Sequential hydrator/committer loop for admitted webhook deliveries.

use crate::config::ProjectSpec;
use crate::graphql::{FetchedRoot, GitHubGraphQLClient};
use crate::mapping::{map_root_delete_from_snapshot, map_root_diff};
use crate::types::{HydratorHealth, RootSnapshot, SnapshotElement, WebhookLocator};
use crate::webhook::{decode_admission_change, warn_unhealthy_hydrator};
use anyhow::{Context, Result};
use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
use drasi_lib::sources::base::SourceBase;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::WalProvider;
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::{sleep, Duration};

const ROOT_SNAPSHOT_PREFIX: &str = "root-snapshot:";
const RECONCILE_INDEX_KEY: &str = "reconcile-index";
const EFFECTIVE_REPOS_KEY: &str = "effective-repos";

pub struct HydratorParams {
    pub source_id: String,
    pub base: SourceBase,
    pub wal: Arc<dyn WalProvider>,
    pub state_store: Arc<dyn StateStoreProvider>,
    pub api_client: Arc<GitHubGraphQLClient>,
    pub projects: Vec<ProjectSpec>,
    pub effective_repos: Arc<RwLock<HashSet<String>>>,
    pub notify: Arc<Notify>,
    pub health: Arc<RwLock<HydratorHealth>>,
    pub processing_gate: Arc<Mutex<()>>,
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
            .oldest_sequence(&params.source_id)
            .await
            .context("WAL oldest_sequence failed")?;

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
            .read_from(&params.source_id, oldest)
            .await
            .context("WAL read_from failed")?;

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
        .with_context(|| format!("Failed to decode admission at sequence {sequence}"))?;
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
        debug!(
            "[{}] Skipping unsupported event type '{}' for delivery {}",
            params.source_id, locator.event_type, delivery_id
        );
        params
            .wal
            .prune_up_to(&params.source_id, sequence)
            .await
            .context("Failed to prune unsupported delivery from WAL")?;
        return Ok(());
    }

    let _processing_guard = params.processing_gate.lock().await;

    if let Some(repo) = locator.repository_full_name.as_ref() {
        if !is_repo_effective(&params.effective_repos, repo).await {
            debug!(
                "[{}] Skipping delivery {} for non-effective repo {}",
                params.source_id, delivery_id, repo
            );
            params
                .wal
                .prune_up_to(&params.source_id, sequence)
                .await
                .context("Failed to prune skipped delivery from WAL")?;
            return Ok(());
        }
    }

    let root_snapshot_key = snapshot_key_for_locator(&locator, None);
    let previous = load_root_snapshot(
        params.state_store.as_ref(),
        &params.source_id,
        &root_snapshot_key,
    )
    .await?;
    if previous
        .as_ref()
        .and_then(|snapshot| snapshot.committed_delivery_id.as_deref())
        == Some(delivery_id.as_str())
    {
        debug!(
            "[{}] Delivery {} already committed after locator hydration, pruning WAL",
            params.source_id, delivery_id
        );
        params
            .wal
            .prune_up_to(&params.source_id, sequence)
            .await
            .with_context(|| {
                format!("Failed to prune already-committed delivery {delivery_id} from WAL")
            })?;
        return Ok(());
    }
    let fetched = params
        .api_client
        .fetch_root_from_locator(&locator)
        .await
        .with_context(|| {
            format!(
                "Failed to hydrate locator event={} action={} node_id={:?}",
                locator.event_type, locator.action, locator.node_id
            )
        })?;
    if fetched.is_none() && !is_authoritative_delete_action(&locator) {
        if has_later_authoritative_delete(params, sequence, &locator).await? {
            debug!(
                "[{}] Treating absent stale delivery {} as converged because a later authoritative delete is durable",
                params.source_id, delivery_id
            );
            params
                .wal
                .prune_up_to(&params.source_id, sequence)
                .await
                .with_context(|| {
                    format!("Failed to prune stale delivery {delivery_id} from WAL")
                })?;
            return Ok(());
        }
        anyhow::bail!(
            "GraphQL returned node=null for non-delete action '{}' (event '{}'); treating as transient",
            locator.action,
            locator.event_type
        );
    }

    if let Some(authoritative_repo) = fetched.as_ref().and_then(FetchedRoot::repository_full_name) {
        if !is_repo_effective(&params.effective_repos, authoritative_repo).await {
            debug!(
                "[{}] Skipping delivery {} because authoritative repository {} is outside effective scope",
                params.source_id, delivery_id, authoritative_repo
            );
            params
                .wal
                .prune_up_to(&params.source_id, sequence)
                .await
                .context("Failed to prune authoritative out-of-scope delivery from WAL")?;
            return Ok(());
        }
    }

    let mut reconcile_index_cache: Option<HashMap<String, SnapshotElement>> = None;
    if is_project_event(&locator) {
        let identity = resolve_project_identity(
            params,
            &locator,
            fetched.as_ref(),
            &mut reconcile_index_cache,
        )
        .await?;
        let Some(identity) = identity else {
            debug!(
                "[{}] Skipping delivery {} because authoritative project identity could not be resolved",
                params.source_id, delivery_id
            );
            params
                .wal
                .prune_up_to(&params.source_id, sequence)
                .await
                .context("Failed to prune skipped project delivery from WAL")?;
            return Ok(());
        };
        if !is_project_configured(&params.projects, &identity.owner, identity.number) {
            debug!(
                "[{}] Skipping delivery {} for unconfigured project id={} owner={} number={}",
                params.source_id, delivery_id, identity.project_id, identity.owner, identity.number
            );
            params
                .wal
                .prune_up_to(&params.source_id, sequence)
                .await
                .context("Failed to prune skipped project delivery from WAL")?;
            return Ok(());
        }
    }

    let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let root_snapshot_key = snapshot_key_for_locator(&locator, fetched.as_ref());
    let previous = load_root_snapshot(
        params.state_store.as_ref(),
        &params.source_id,
        &root_snapshot_key,
    )
    .await?;

    if let Some(root) = fetched {
        let (changes, mut next_snapshot) =
            map_root_diff(&params.source_id, &root, previous.as_ref(), effective_from)?;
        debug!(
            "[{}] Hydrated root {} ({}) produced {} change(s)",
            params.source_id,
            root.root_id(),
            root.root_kind(),
            changes.len()
        );
        dispatch_changes(&params.base, &params.source_id, changes).await?;

        let mut reconcile_index =
            load_reconcile_index(params.state_store.as_ref(), &params.source_id).await?;
        synchronize_reconcile_index(&mut reconcile_index, previous.as_ref(), &next_snapshot);
        save_reconcile_index(
            params.state_store.as_ref(),
            &params.source_id,
            &reconcile_index,
        )
        .await?;

        next_snapshot.committed_delivery_id = Some(delivery_id.clone());
        next_snapshot.committed_sequence = Some(sequence);
        save_root_snapshot(
            params.state_store.as_ref(),
            &params.source_id,
            &root_snapshot_key,
            &next_snapshot,
        )
        .await?;
    } else {
        let mut reconcile_index = match reconcile_index_cache.take() {
            Some(index) => index,
            None => load_reconcile_index(params.state_store.as_ref(), &params.source_id).await?,
        };

        let (changes, deleted_ids) = if previous.is_some() {
            let changes =
                map_root_delete_from_snapshot(&params.source_id, previous.as_ref(), effective_from);
            let deleted_ids = deleted_element_ids(&changes);
            (changes, deleted_ids)
        } else if let Some(root_id) = resolve_delete_root_id(&locator, &reconcile_index) {
            map_delete_from_reconcile_index(
                &params.source_id,
                &root_id,
                &reconcile_index,
                effective_from,
            )
        } else {
            (Vec::new(), HashSet::new())
        };

        debug!(
            "[{}] Locator {:?} resolved to delete with {} change(s)",
            params.source_id,
            locator.node_id,
            changes.len()
        );
        dispatch_changes(&params.base, &params.source_id, changes).await?;

        if !deleted_ids.is_empty() {
            for id in &deleted_ids {
                reconcile_index.remove(id);
            }
            save_reconcile_index(
                params.state_store.as_ref(),
                &params.source_id,
                &reconcile_index,
            )
            .await?;
        }

        let tombstone = RootSnapshot {
            root_id: locator
                .node_id
                .clone()
                .or(locator.project_id.clone())
                .or(locator.repository_full_name.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            root_kind: "Deleted".to_string(),
            repository_full_name: locator.repository_full_name.clone(),
            elements: HashMap::new(),
            committed_delivery_id: Some(delivery_id.clone()),
            committed_sequence: Some(sequence),
        };
        save_root_snapshot(
            params.state_store.as_ref(),
            &params.source_id,
            &root_snapshot_key,
            &tombstone,
        )
        .await?;
    }

    params
        .wal
        .prune_up_to(&params.source_id, sequence)
        .await
        .with_context(|| format!("Failed to prune WAL admission for delivery {delivery_id}"))?;

    Ok(())
}

async fn dispatch_changes(
    base: &SourceBase,
    source_id: &str,
    changes: Vec<drasi_core::models::SourceChange>,
) -> Result<()> {
    for change in changes {
        let wrapper = SourceEventWrapper::new(
            source_id.to_string(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );
        base.dispatch_event(wrapper)
            .await
            .context("Failed to dispatch hydrated SourceChange")?;
    }
    Ok(())
}

async fn is_repo_effective(effective_repos: &Arc<RwLock<HashSet<String>>>, repo: &str) -> bool {
    let repo = repo.to_ascii_lowercase();
    effective_repos.read().await.contains(&repo)
}

async fn has_later_authoritative_delete(
    params: &HydratorParams,
    sequence: u64,
    locator: &WebhookLocator,
) -> Result<bool> {
    let entries = params
        .wal
        .read_from(&params.source_id, sequence.saturating_add(1))
        .await
        .context("Failed to inspect later WAL admissions")?;

    Ok(entries.into_iter().any(|(later_sequence, change)| {
        if later_sequence <= sequence {
            return false;
        }
        let Ok((_, later_locator)) = decode_admission_change(&change) else {
            return false;
        };
        is_authoritative_delete_action(&later_locator)
            && locators_identify_same_root(locator, &later_locator)
    }))
}

fn locators_identify_same_root(left: &WebhookLocator, right: &WebhookLocator) -> bool {
    if left.event_type != right.event_type {
        return false;
    }
    match (
        left.node_id.as_deref(),
        right.node_id.as_deref(),
        left.project_id.as_deref(),
        right.project_id.as_deref(),
        left.repository_full_name.as_deref(),
        right.repository_full_name.as_deref(),
    ) {
        (Some(left), Some(right), _, _, _, _) => left == right,
        (_, _, Some(left), Some(right), _, _) => left == right,
        (_, _, _, _, Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => false,
    }
}

fn synchronize_reconcile_index(
    reconcile_index: &mut HashMap<String, SnapshotElement>,
    previous: Option<&RootSnapshot>,
    next: &RootSnapshot,
) {
    if let Some(previous) = previous {
        for id in previous.elements.keys() {
            if !next.elements.contains_key(id) {
                reconcile_index.remove(id);
            }
        }
    }
    for (id, element) in &next.elements {
        reconcile_index.insert(id.clone(), element.clone());
    }
}

fn is_project_event(locator: &WebhookLocator) -> bool {
    matches!(
        locator.event_type.as_str(),
        "projects_v2" | "projects_v2_item"
    )
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

fn is_project_configured(projects: &[ProjectSpec], owner: &str, number: u32) -> bool {
    projects
        .iter()
        .any(|project| project.owner.eq_ignore_ascii_case(owner) && project.number == number)
}

#[derive(Debug, Clone)]
struct ProjectIdentity {
    project_id: String,
    owner: String,
    number: u32,
}

async fn resolve_project_identity(
    params: &HydratorParams,
    locator: &WebhookLocator,
    fetched: Option<&FetchedRoot>,
    reconcile_index_cache: &mut Option<HashMap<String, SnapshotElement>>,
) -> Result<Option<ProjectIdentity>> {
    if let Some(identity) = project_identity_from_fetched(fetched)? {
        return Ok(Some(identity));
    }

    let project_id_hint = locator
        .project_id
        .as_deref()
        .or_else(|| {
            (locator.event_type == "projects_v2")
                .then_some(locator.node_id.as_deref())
                .flatten()
        })
        .map(str::to_string);

    if let Some(project_id) = project_id_hint {
        if let Some(project) = params.api_client.fetch_project(&project_id).await? {
            let number = u32::try_from(project.number)
                .context("Project number does not fit in u32 while resolving project scope")?;
            return Ok(Some(ProjectIdentity {
                project_id: project.id,
                owner: project.owner.login,
                number,
            }));
        }

        if reconcile_index_cache.is_none() {
            *reconcile_index_cache =
                Some(load_reconcile_index(params.state_store.as_ref(), &params.source_id).await?);
        }
        if let Some(index) = reconcile_index_cache.as_ref() {
            if let Some(identity) = project_identity_from_reconcile_index(index, &project_id) {
                return Ok(Some(identity));
            }
        }
    }

    Ok(None)
}

fn project_identity_from_fetched(fetched: Option<&FetchedRoot>) -> Result<Option<ProjectIdentity>> {
    let Some(root) = fetched else {
        return Ok(None);
    };

    match root {
        FetchedRoot::Project(project) => {
            let number = u32::try_from(project.number)
                .context("Project number does not fit in u32 while resolving project scope")?;
            Ok(Some(ProjectIdentity {
                project_id: project.id.clone(),
                owner: project.owner.login.clone(),
                number,
            }))
        }
        FetchedRoot::ProjectItem(item) => {
            let number = u32::try_from(item.project.number).context(
                "Project item parent number does not fit in u32 while resolving project scope",
            )?;
            Ok(Some(ProjectIdentity {
                project_id: item.project.id.clone(),
                owner: item.project.owner.login.clone(),
                number,
            }))
        }
        _ => Ok(None),
    }
}

fn project_identity_from_reconcile_index(
    index: &HashMap<String, SnapshotElement>,
    project_id: &str,
) -> Option<ProjectIdentity> {
    let project = index.get(project_id)?;
    if project.element_type != "node"
        || !project.labels.iter().any(|label| label == "GitHubProject")
    {
        return None;
    }
    let owner = project.properties.get("owner")?.as_str()?.to_string();
    let number = project
        .properties
        .get("number")
        .and_then(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
        .or_else(|| {
            project
                .properties
                .get("number")
                .and_then(|v| v.as_i64().and_then(|n| u32::try_from(n).ok()))
        })?;
    Some(ProjectIdentity {
        project_id: project.id.clone(),
        owner,
        number,
    })
}

fn resolve_delete_root_id(
    locator: &WebhookLocator,
    reconcile_index: &HashMap<String, SnapshotElement>,
) -> Option<String> {
    if let Some(id) = locator.node_id.as_ref() {
        return Some(id.clone());
    }
    if let Some(id) = locator.project_id.as_ref() {
        return Some(id.clone());
    }
    if locator.event_type == "repository" {
        let repo = locator
            .repository_full_name
            .as_deref()?
            .to_ascii_lowercase();
        if let Some((id, _)) = reconcile_index.iter().find(|(_, element)| {
            element.element_type == "node"
                && element
                    .labels
                    .iter()
                    .any(|label| label == "GitHubRepository")
                && element
                    .properties
                    .get("nameWithOwner")
                    .and_then(|v| v.as_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(&repo))
        }) {
            return Some(id.clone());
        }
    }
    None
}

fn map_delete_from_reconcile_index(
    source_id: &str,
    root_id: &str,
    reconcile_index: &HashMap<String, SnapshotElement>,
    effective_from: u64,
) -> (Vec<SourceChange>, HashSet<String>) {
    let mut relations_by_in: HashMap<&str, Vec<&SnapshotElement>> = HashMap::new();
    let mut relations_by_out: HashMap<&str, Vec<&SnapshotElement>> = HashMap::new();
    for element in reconcile_index.values() {
        if element.element_type != "relation" {
            continue;
        }
        if let Some(in_id) = element.in_node_id.as_deref() {
            relations_by_in.entry(in_id).or_default().push(element);
        }
        if let Some(out_id) = element.out_node_id.as_deref() {
            relations_by_out.entry(out_id).or_default().push(element);
        }
    }

    let mut queue = VecDeque::from([root_id.to_string()]);
    let mut visited_nodes = HashSet::new();
    let mut included_ids = HashSet::new();

    while let Some(node_id) = queue.pop_front() {
        if !visited_nodes.insert(node_id.clone()) {
            continue;
        }

        if let Some(node) = reconcile_index.get(&node_id) {
            if node.element_type == "node" {
                included_ids.insert(node.id.clone());
            }
        }

        if let Some(rels) = relations_by_out.get(node_id.as_str()) {
            for rel in rels {
                included_ids.insert(rel.id.clone());
            }
        }

        if let Some(rels) = relations_by_in.get(node_id.as_str()) {
            for rel in rels {
                included_ids.insert(rel.id.clone());
                if let Some(child_id) = rel.out_node_id.as_ref() {
                    if !visited_nodes.contains(child_id) {
                        queue.push_back(child_id.clone());
                    }
                }
            }
        }
    }

    let mut delete_ids: Vec<String> = included_ids.iter().cloned().collect();
    delete_ids.sort();
    let mut changes = Vec::with_capacity(delete_ids.len());
    for id in &delete_ids {
        if let Some(element) = reconcile_index.get(id) {
            changes.push(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new(source_id, &element.id),
                    labels: element
                        .labels
                        .iter()
                        .map(|label| Arc::<str>::from(label.as_str()))
                        .collect::<Vec<_>>()
                        .into(),
                    effective_from,
                },
            });
        }
    }

    (changes, included_ids)
}

fn deleted_element_ids(changes: &[SourceChange]) -> HashSet<String> {
    changes
        .iter()
        .filter_map(|change| match change {
            SourceChange::Delete { metadata } => {
                Some(metadata.reference.element_id.as_ref().to_string())
            }
            _ => None,
        })
        .collect()
}

fn is_authoritative_delete_action(locator: &WebhookLocator) -> bool {
    let action = locator.action.as_str();
    match locator.event_type.as_str() {
        "issues" => matches!(action, "deleted" | "transferred"),
        "issue_comment" => matches!(action, "deleted"),
        "pull_request_review_comment" => matches!(action, "deleted"),
        "pull_request_review" => matches!(action, "dismissed"),
        "projects_v2_item" | "project_item" => matches!(action, "deleted" | "archived"),
        "projects_v2" | "project" => matches!(action, "deleted"),
        "repository" => matches!(action, "deleted" | "archived"),
        _ => false,
    }
}

pub fn snapshot_key_for_locator(locator: &WebhookLocator, fetched: Option<&FetchedRoot>) -> String {
    if let Some(root) = fetched {
        return format!("{ROOT_SNAPSHOT_PREFIX}{}", root.root_id());
    }

    if let Some(node_id) = locator.node_id.as_ref() {
        return format!("{ROOT_SNAPSHOT_PREFIX}{node_id}");
    }
    if let Some(project_id) = locator.project_id.as_ref() {
        return format!("{ROOT_SNAPSHOT_PREFIX}{project_id}");
    }
    if let Some(repo) = locator.repository_full_name.as_ref() {
        return format!("{ROOT_SNAPSHOT_PREFIX}repo:{}", repo.to_ascii_lowercase());
    }
    format!("{ROOT_SNAPSHOT_PREFIX}unknown")
}

pub async fn load_root_snapshot(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    key: &str,
) -> Result<Option<RootSnapshot>> {
    let bytes = state_store
        .get(source_id, key)
        .await
        .map_err(|e| anyhow::anyhow!("state_store.get({key}) failed: {e}"))?;
    match bytes {
        Some(data) => Ok(Some(
            serde_json::from_slice(&data).context("Failed to deserialize root snapshot")?,
        )),
        None => Ok(None),
    }
}

pub async fn save_root_snapshot(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    key: &str,
    snapshot: &RootSnapshot,
) -> Result<()> {
    let data = serde_json::to_vec(snapshot).context("Failed to serialize root snapshot")?;
    state_store
        .set(source_id, key, data)
        .await
        .map_err(|e| anyhow::anyhow!("state_store.set({key}) failed: {e}"))
}

pub async fn delete_root_snapshot(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    key: &str,
) -> Result<()> {
    state_store
        .delete(source_id, key)
        .await
        .map_err(|e| anyhow::anyhow!("state_store.delete({key}) failed: {e}"))?;
    Ok(())
}

pub async fn load_reconcile_index(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
) -> Result<HashMap<String, SnapshotElement>> {
    let bytes = state_store
        .get(source_id, RECONCILE_INDEX_KEY)
        .await
        .map_err(|e| anyhow::anyhow!("state_store.get({RECONCILE_INDEX_KEY}) failed: {e}"))?;
    match bytes {
        Some(data) => Ok(
            serde_json::from_slice::<HashMap<String, SnapshotElement>>(&data)
                .context("Failed to parse reconcile index")?,
        ),
        None => Ok(HashMap::new()),
    }
}

pub async fn save_reconcile_index(
    state_store: &dyn StateStoreProvider,
    source_id: &str,
    index: &HashMap<String, SnapshotElement>,
) -> Result<()> {
    let payload = serde_json::to_vec(index).context("Failed to serialize reconcile index")?;
    state_store
        .set(source_id, RECONCILE_INDEX_KEY, payload)
        .await
        .map_err(|e| anyhow::anyhow!("state_store.set({RECONCILE_INDEX_KEY}) failed: {e}"))
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
