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

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use std::{pin::Pin, sync::Arc};

use super::{ChangeEnvelope, StreamId};

pub struct BootstrapWatermark {
    pub stream: StreamId,
    pub source_id: Option<String>,
    pub sequence: u64,
    pub position: Option<Bytes>,
}

pub struct ComputationBootstrapSnapshot {
    pub changes: Pin<Box<dyn Stream<Item = anyhow::Result<ChangeEnvelope>> + Send>>,
    pub watermarks: Vec<BootstrapWatermark>,
}

/// A scoped snapshot provider. Dropping its stream must cancel its work; provider
/// resources requiring asynchronous cleanup must be registered with the graph.
#[async_trait]
pub trait ComputationBootstrapProvider: Send + Sync {
    /// Establish source subscription readiness before deciding whether persisted
    /// query state can be reused. This must not consume snapshot records.
    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        Ok(BootstrapPreparation::Ready)
    }
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot>;
    /// Boundaries that become available only after the snapshot stream closes.
    async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapPreparation {
    Ready,
    RefreshVolatile,
    ResetRequired,
}

pub struct QueryBootstrapResource(pub Arc<dyn ComputationBootstrapProvider>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryRecoveryPolicy {
    #[default]
    Strict,
    AutoReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryPublicationMode {
    #[default]
    Atomic,
    /// Explicit compatibility mode: checkpoint/core commit precedes publication.
    /// A durable pending marker fences uncertain output until reset/recovery.
    NonAtomic,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QueryOptions {
    pub recovery: QueryRecoveryPolicy,
    pub publication: QueryPublicationMode,
}

#[derive(Debug, thiserror::Error)]
pub enum QueryRecoveryError {
    #[error("source subscription requires reset/rebootstrap before this query can resume")]
    SourceResetRequired,
    #[error("query configuration changed; explicit reset/rebootstrap is required")]
    ConfigurationChanged,
    #[error("query bootstrap did not complete; explicit reset/rebootstrap is required")]
    IncompleteBootstrap,
    #[error("non-atomic query output publication is incomplete")]
    PendingPublication,
    #[error("query reset did not complete")]
    IncompleteReset,
    #[error("inconsistent durable query output: {0}")]
    Inconsistent(String),
}
