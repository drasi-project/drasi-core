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

//! Convergence of the configured worker file into the worker/slot graph.
//!
//! One [`WorkerSync`] instance is shared by the two live entry points:
//!
//! * `start()` performs one convergence so a restarted Source re-states the
//!   configured capacity even if `push` deliveries were missed while it was
//!   down, and so the retirement ledger below is seeded.
//! * the webhook ingress converges again whenever a `push` touches the exact
//!   configured repository, ref, and path.
//!
//! Both paths reuse [`crate::mapping::worker_changes`], which the bootstrapper
//! also uses, so a configuration that bootstrap projects one way can never be
//! projected a different way by a live delivery.
//!
//! # The retirement ledger
//!
//! Reducing `slots` must not delete a slot node that an in-flight Lease still
//! references. The Source therefore records, per worker, the highest slot
//! number it has ever projected, and keeps projecting slots above the new
//! configured count as `enabled = false, retiring = true`. The ledger lives in
//! the Source's own durable state store, next to the delivery dedupe markers.
//!
//! The ledger is Source-local by design, and that is its documented bounded
//! limitation: a clean bootstrap builds a fresh snapshot from the configured
//! file alone, so slots retired before that bootstrap are not re-materialized.

use crate::config::WorkerConfig;
use crate::mapping::{worker_changes, WorkerProjection};
use crate::worker_client::{WorkerFileClient, WorkerFileError};
use crate::workers::{parse_worker_file, WorkerFileLocation};
use anyhow::{Context, Result};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::{WalError, WalProvider};
use log::{error, info, warn};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const REGISTRY_KEY: &str = "workers:registry";
/// GitHub caps the `commits` array it delivers with a `push`. A payload at or
/// above the cap may be truncated, so it can never prove the worker file was
/// left alone.
const PUSH_COMMIT_CAP: usize = 20;

/// Why one convergence attempt did not complete.
#[derive(Debug)]
pub enum WorkerSyncError {
    /// The worker file could not be read at all (transport, authentication, or
    /// a server-side failure). Nothing is asserted about worker capacity; the
    /// caller retries or fails the component.
    Unavailable(anyhow::Error),
    /// The durable Source state or WAL could not be used.
    Storage(anyhow::Error),
}

impl std::fmt::Display for WorkerSyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(formatter, "{error:#}"),
            Self::Storage(error) => write!(formatter, "{error:#}"),
        }
    }
}

/// What one convergence changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSyncOutcome {
    /// Number of graph changes appended to the WAL.
    pub appended: usize,
    /// True when the configured file was fetched and strictly validated.
    pub accepted: bool,
}

pub struct WorkerSync {
    source_id: String,
    location: WorkerFileLocation,
    client: WorkerFileClient,
    state_store: Arc<dyn StateStoreProvider>,
    wal: Arc<dyn WalProvider>,
    // Convergence reads the ledger, projects, appends, then writes the ledger
    // back. Serializing the whole sequence keeps a start-up convergence and a
    // concurrent `push` delivery from interleaving into a torn ledger.
    gate: Mutex<()>,
}

impl WorkerSync {
    pub fn new(
        source_id: String,
        config: &WorkerConfig,
        state_store: Arc<dyn StateStoreProvider>,
        wal: Arc<dyn WalProvider>,
    ) -> Result<Self> {
        Ok(Self {
            source_id,
            location: config.location(),
            client: WorkerFileClient::new(&config.token, &config.api_base_url)?,
            state_store,
            wal,
            gate: Mutex::new(()),
        })
    }

    pub fn location(&self) -> &WorkerFileLocation {
        &self.location
    }

    /// Fetch, validate, and project the configured worker file.
    pub async fn converge(&self) -> Result<WorkerSyncOutcome, WorkerSyncError> {
        let _guard = self.gate.lock().await;
        let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;

        let content = match self.client.fetch(&self.location).await {
            Ok(content) => content,
            Err(WorkerFileError::Unavailable(error)) => {
                return Err(WorkerSyncError::Unavailable(error))
            }
            Err(WorkerFileError::Rejected(error)) => {
                error!(
                    "[{}] worker file at '{}' ref '{}' path '{}' rejected [{}]: {}",
                    self.source_id,
                    self.location.repository,
                    self.location.r#ref,
                    self.location.path,
                    error.code,
                    error.message
                );
                let changes = worker_changes(
                    &self.source_id,
                    effective_from,
                    &self.location,
                    &WorkerProjection::Rejected(&error),
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                );
                let appended = self.append(&changes).await?;
                return Ok(WorkerSyncOutcome {
                    appended,
                    accepted: false,
                });
            }
        };

        let file = match parse_worker_file(&content.text) {
            Ok(file) => file,
            Err(error) => {
                error!(
                    "[{}] worker file at '{}' ref '{}' path '{}' rejected [{}]: {}",
                    self.source_id,
                    self.location.repository,
                    self.location.r#ref,
                    self.location.path,
                    error.code,
                    error.message
                );
                let changes = worker_changes(
                    &self.source_id,
                    effective_from,
                    &self.location,
                    &WorkerProjection::Rejected(&error),
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                );
                let appended = self.append(&changes).await?;
                return Ok(WorkerSyncOutcome {
                    appended,
                    accepted: false,
                });
            }
        };

        let previous = self.read_registry().await?;
        let configured: BTreeMap<String, u32> = file
            .workers
            .iter()
            .map(|worker| (worker.worker_id.clone(), worker.slots))
            .collect();
        let removed: BTreeMap<String, u32> = previous
            .iter()
            .filter(|(worker_id, _)| !configured.contains_key(*worker_id))
            .map(|(worker_id, slots)| (worker_id.clone(), *slots))
            .collect();

        let changes = worker_changes(
            &self.source_id,
            effective_from,
            &self.location,
            &WorkerProjection::Loaded {
                file: &file,
                content: &content,
            },
            &previous,
            &removed,
        );
        let appended = self.append(&changes).await?;

        // The next ledger keeps the high-water slot count of every still
        // configured worker, so a later reduction still knows which excess
        // slots must stay addressable. Removed workers leave the ledger.
        let next: BTreeMap<String, u32> = configured
            .iter()
            .map(|(worker_id, slots)| {
                let high_water = (*previous.get(worker_id).unwrap_or(&0)).max(*slots);
                (worker_id.clone(), high_water)
            })
            .collect();
        self.write_registry(&next).await?;

        info!(
            "[{}] converged {} configured worker(s) from '{}' ref '{}' path '{}' ({appended} \
             change(s))",
            self.source_id,
            file.workers.len(),
            self.location.repository,
            self.location.r#ref,
            self.location.path
        );
        Ok(WorkerSyncOutcome {
            appended,
            accepted: true,
        })
    }

    async fn append(
        &self,
        changes: &[drasi_core::models::SourceChange],
    ) -> Result<usize, WorkerSyncError> {
        for change in changes {
            match self.wal.append(&self.source_id, change).await {
                Ok(_) => {}
                Err(WalError::CapacityExhausted(message)) => {
                    return Err(WorkerSyncError::Storage(anyhow::anyhow!(
                        "source WAL capacity exhausted while projecting workers: {message}"
                    )))
                }
                Err(error) => {
                    return Err(WorkerSyncError::Storage(anyhow::anyhow!(
                        "WAL append failed while projecting workers: {error}"
                    )))
                }
            }
        }
        Ok(changes.len())
    }

    async fn read_registry(&self) -> Result<BTreeMap<String, u32>, WorkerSyncError> {
        let stored = self
            .state_store
            .get(&self.source_id, REGISTRY_KEY)
            .await
            .context("failed to read the worker retirement ledger")
            .map_err(WorkerSyncError::Storage)?;
        let Some(stored) = stored else {
            return Ok(BTreeMap::new());
        };
        match serde_json::from_slice(&stored) {
            Ok(registry) => Ok(registry),
            Err(error) => {
                // A ledger we cannot read must not block convergence; the only
                // consequence is that previously retired slots are not
                // re-stated, exactly as after a clean bootstrap.
                warn!(
                    "[{}] worker retirement ledger is unreadable ({error}); rebuilding it",
                    self.source_id
                );
                Ok(BTreeMap::new())
            }
        }
    }

    async fn write_registry(
        &self,
        registry: &BTreeMap<String, u32>,
    ) -> Result<(), WorkerSyncError> {
        let encoded = serde_json::to_vec(registry)
            .context("failed to encode the worker retirement ledger")
            .map_err(WorkerSyncError::Storage)?;
        self.state_store
            .set(&self.source_id, REGISTRY_KEY, encoded)
            .await
            .context("failed to persist the worker retirement ledger")
            .map_err(WorkerSyncError::Storage)
    }
}

/// True when a `push` delivery could have changed the configured worker file.
///
/// GitHub caps the `commits` array of a large push, so an unprovable push
/// converges rather than being ignored: re-reading the file is idempotent,
/// while missing a change would leave stale capacity in the graph.
pub fn push_touches_worker_file(payload: &serde_json::Value, path: &str) -> bool {
    // A branch/tag create, a delete, and a force-push all rewrite what the
    // configured ref resolves to without necessarily listing the worker file
    // among any commit's changed paths. None of them can prove the file is
    // unchanged, so all of them converge.
    let flag = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    if flag("created") || flag("deleted") || flag("forced") {
        return true;
    }
    let Some(commits) = payload.get("commits").and_then(serde_json::Value::as_array) else {
        return true;
    };
    // `size` is the true commit count when GitHub sends it, and the delivered
    // array is capped independently of it. Either signal of truncation means
    // the payload cannot prove the file was untouched.
    let truncated = payload
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|size| size > commits.len() as u64)
        || commits.len() >= PUSH_COMMIT_CAP;
    if truncated {
        return true;
    }
    let head = payload
        .get("head_commit")
        .filter(|head| !head.is_null())
        .into_iter();
    commits
        .iter()
        .chain(head)
        .any(|commit| commit_touches(commit, path))
}

fn commit_touches(commit: &serde_json::Value, path: &str) -> bool {
    ["added", "modified", "removed"].iter().any(|key| {
        commit
            .get(key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .any(|entry| entry == path)
            })
    })
}
