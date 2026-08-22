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

//! Convergence of the configured agent file into the agent/slot graph.
//!
//! One [`AgentSync`] instance is shared by the two live entry points:
//!
//! * `start()` performs one convergence so a restarted Source re-states the
//!   configured capacity even if `push` deliveries were missed while it was
//!   down, and so the retirement ledger below is seeded.
//! * the webhook ingress converges again whenever a `push` touches the exact
//!   configured repository, ref, and path.
//!
//! Both paths reuse [`crate::mapping::agent_changes`], which the bootstrapper
//! also uses, so a configuration that bootstrap projects one way can never be
//! projected a different way by a live delivery.
//!
//! # The retirement ledger
//!
//! Reducing `slots` must not delete a slot node that an in-flight Lease still
//! references. The Source therefore records, per agent, the highest slot
//! number it has ever projected, and keeps projecting slots above the new
//! configured count as `enabled = false, retiring = true`. The ledger lives in
//! the Source's own durable state store, next to the delivery dedupe markers.
//!
//! The ledger is Source-local by design, and that is its documented bounded
//! limitation: a clean bootstrap builds a fresh snapshot from the configured
//! file alone, so slots retired before that bootstrap are not re-materialized.

use crate::agent_client::{AgentFileClient, AgentFileError};
use crate::agents::{parse_agent_file, AgentFileLocation};
use crate::config::AgentConfig;
use crate::lease_ledger::Allocator;
use crate::mapping::{agent_changes, AgentProjection};
use anyhow::Result;
use log::{error, info};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// GitHub caps the `commits` array it delivers with a `push`. A payload at or
/// above the cap may be truncated, so it can never prove the agent file was
/// left alone.
const PUSH_COMMIT_CAP: usize = 20;

/// Why one convergence attempt did not complete.
#[derive(Debug)]
pub enum AgentSyncError {
    /// The agent file could not be read at all (transport, authentication, or
    /// a server-side failure). Nothing is asserted about agent capacity; the
    /// caller retries or fails the component.
    Unavailable(anyhow::Error),
    /// The durable Source state or WAL could not be used.
    Storage(anyhow::Error),
}

impl std::fmt::Display for AgentSyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(formatter, "{error:#}"),
            Self::Storage(error) => write!(formatter, "{error:#}"),
        }
    }
}

/// What one convergence changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSyncOutcome {
    /// Number of graph changes appended to the WAL.
    pub appended: usize,
    /// True when the configured file was fetched and strictly validated.
    pub accepted: bool,
}

pub struct AgentSync {
    source_id: String,
    location: AgentFileLocation,
    client: AgentFileClient,
    allocator: Arc<Allocator>,
    // Convergence reads the ledger, projects, appends, then writes the ledger
    // back. Serializing the whole sequence keeps a start-up convergence and a
    // concurrent `push` delivery from interleaving into a torn ledger.
    gate: Mutex<()>,
}

impl AgentSync {
    pub fn new(source_id: String, config: &AgentConfig, allocator: Arc<Allocator>) -> Result<Self> {
        Ok(Self {
            source_id,
            location: config.location(),
            client: AgentFileClient::new(&config.token, &config.api_base_url)?,
            allocator,
            gate: Mutex::new(()),
        })
    }

    pub fn location(&self) -> &AgentFileLocation {
        &self.location
    }

    /// Fetch, validate, and project the configured agent file.
    pub async fn converge(&self) -> Result<AgentSyncOutcome, AgentSyncError> {
        let _guard = self.gate.lock().await;
        let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;

        let content = match self.client.fetch(&self.location).await {
            Ok(content) => content,
            Err(AgentFileError::Unavailable(error)) => {
                return Err(AgentSyncError::Unavailable(error))
            }
            Err(AgentFileError::Rejected(error)) => {
                error!(
                    "[{}] agent file at '{}' ref '{}' path '{}' rejected [{}]: {}",
                    self.source_id,
                    self.location.repository,
                    self.location.r#ref,
                    self.location.path,
                    error.code,
                    error.message
                );
                let changes = agent_changes(
                    &self.source_id,
                    effective_from,
                    &self.location,
                    &AgentProjection::Rejected(&error),
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                );
                let appended = self
                    .allocator
                    .append_projection(&changes)
                    .await
                    .map_err(AgentSyncError::Storage)?;
                return Ok(AgentSyncOutcome {
                    appended,
                    accepted: false,
                });
            }
        };

        let file = match parse_agent_file(&content.text) {
            Ok(file) => file,
            Err(error) => {
                error!(
                    "[{}] agent file at '{}' ref '{}' path '{}' rejected [{}]: {}",
                    self.source_id,
                    self.location.repository,
                    self.location.r#ref,
                    self.location.path,
                    error.code,
                    error.message
                );
                let changes = agent_changes(
                    &self.source_id,
                    effective_from,
                    &self.location,
                    &AgentProjection::Rejected(&error),
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                );
                let appended = self
                    .allocator
                    .append_projection(&changes)
                    .await
                    .map_err(AgentSyncError::Storage)?;
                return Ok(AgentSyncOutcome {
                    appended,
                    accepted: false,
                });
            }
        };

        let appended = self
            .allocator
            .sync_agents(&self.location, &file, &content, effective_from)
            .await
            .map_err(AgentSyncError::Storage)?;

        info!(
            "[{}] converged {} configured agent(s) from '{}' ref '{}' path '{}' ({appended} \
             change(s))",
            self.source_id,
            file.agents.len(),
            self.location.repository,
            self.location.r#ref,
            self.location.path
        );
        Ok(AgentSyncOutcome {
            appended,
            accepted: true,
        })
    }
}

/// True when a `push` delivery could have changed the configured agent file.
///
/// GitHub caps the `commits` array of a large push, so an unprovable push
/// converges rather than being ignored: re-reading the file is idempotent,
/// while missing a change would leave stale capacity in the graph.
pub fn push_touches_agent_file(payload: &serde_json::Value, path: &str) -> bool {
    // A branch/tag create, a delete, and a force-push all rewrite what the
    // configured ref resolves to without necessarily listing the agent file
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
