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

use super::{ComponentId, StreamId};
use drasi_core::interface::SourceCheckpoint;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceProgressKey {
    Source(String),
    Stream(StreamId),
}

/// A read-only view of an owning query's committed input progress. Persistence
/// comes from that query's actual checkpoint provider, not from this watch channel.
#[derive(Debug, Clone, Default)]
pub struct SourceProgressSnapshot {
    pub ready: bool,
    pub persistent: bool,
    pub reset_generation: u64,
    pub checkpoints: BTreeMap<SourceProgressKey, SourceCheckpoint>,
    pub failure: Option<Arc<str>>,
}

pub struct QuerySourceProgress {
    graph_id: Arc<str>,
    query_id: ComponentId,
    state: watch::Sender<Arc<SourceProgressSnapshot>>,
}

impl QuerySourceProgress {
    pub fn new(graph_id: impl Into<Arc<str>>, query_id: ComponentId) -> super::Result<Self> {
        let graph_id = graph_id.into();
        super::data::validate_identifier("graph", &graph_id)?;
        Ok(Self {
            graph_id,
            query_id,
            state: watch::channel(Arc::new(SourceProgressSnapshot::default())).0,
        })
    }
    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }
    pub fn query_id(&self) -> &ComponentId {
        &self.query_id
    }
    pub fn snapshot(&self) -> Arc<SourceProgressSnapshot> {
        self.state.borrow().clone()
    }
    pub async fn wait_ready(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        let mut receiver = self.state.subscribe();
        let snapshot = receiver
            .wait_for(|snapshot| snapshot.ready || snapshot.failure.is_some())
            .await?;
        if let Some(failure) = &snapshot.failure {
            anyhow::bail!("{failure}");
        }
        Ok(snapshot.clone())
    }
    pub(super) fn pending(&self) {
        self.state.send_modify(|state| {
            let state = Arc::make_mut(state);
            state.ready = false;
            state.failure = None;
        });
    }
    pub(super) fn fail(&self) {
        self.state.send_modify(|state| {
            let state = Arc::make_mut(state);
            state.ready = false;
            state.failure = Some(Arc::from(
                "owning query progress is fenced after interrupted or failed processing",
            ));
        });
    }
    pub(super) fn publish(&self, snapshot: SourceProgressSnapshot) {
        self.state.send_replace(Arc::new(snapshot));
    }
    pub(super) fn confirm(&self, key: SourceProgressKey, checkpoint: SourceCheckpoint) {
        self.state.send_modify(|state| {
            Arc::make_mut(state).checkpoints.insert(key, checkpoint);
        });
    }
}

/// Register this actual resource for both its owning query and compatible sources.
/// It never registers a legacy Source position handle or controls another pipeline.
pub struct QuerySourceProgressResource(pub Arc<QuerySourceProgress>);
