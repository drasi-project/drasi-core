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
use drasi_core::{
    computation::ComputationQuery,
    interface::{IndexError, StorageDurability},
};
use futures::Stream;
use std::{pin::Pin, sync::Arc};

use super::{ChangeEnvelope, StreamId};

pub(super) const BOOTSTRAP_STATE: &str = "\0computation:query-bootstrap-state:v1";
pub const MAX_BOOTSTRAP_STATE_BYTES: usize = 65536;

/// Bounded connector handover metadata in the query's own transaction domain.
/// Writes finish before external initialization effects may begin. State survives
/// an explicit query reset, so a connector can identify its unfinished resources.
/// Query deprovisioning removes it. The provider must not retain this borrowed service.
#[async_trait]
pub trait BootstrapState: Send + Sync {
    fn durability(&self) -> StorageDurability;
    async fn read(&self) -> anyhow::Result<Option<Bytes>>;
    async fn write(&self, state: Bytes) -> anyhow::Result<()>;
}

pub(super) struct QueryBootstrapState<'a>(pub &'a ComputationQuery);

pub(super) fn validate_bootstrap_state(state: &Bytes) -> anyhow::Result<()> {
    anyhow::ensure!(
        !state.is_empty() && state.len() <= MAX_BOOTSTRAP_STATE_BYTES,
        "bootstrap handover state must contain 1..={MAX_BOOTSTRAP_STATE_BYTES} bytes"
    );
    Ok(())
}

#[async_trait]
impl BootstrapState for QueryBootstrapState<'_> {
    fn durability(&self) -> StorageDurability {
        self.0.resources().durability()
    }

    async fn read(&self) -> anyhow::Result<Option<Bytes>> {
        let Some(checkpoint) = self.0.resources().checkpoint_store() else {
            return Ok(None);
        };
        let Some(state) = checkpoint.read_checkpoint(BOOTSTRAP_STATE).await? else {
            return Ok(None);
        };
        anyhow::ensure!(
            state.sequence == 1,
            "invalid bootstrap handover state version"
        );
        let state = state
            .source_position
            .ok_or_else(|| anyhow::anyhow!("bootstrap handover state is missing"))?;
        validate_bootstrap_state(&state)?;
        Ok(Some(state))
    }

    async fn write(&self, state: Bytes) -> anyhow::Result<()> {
        validate_bootstrap_state(&state)?;
        self.0.resources().atomic_result_transaction()?;
        let checkpoint = self
            .0
            .resources()
            .checkpoint_store()
            .ok_or(IndexError::NotSupported)?;
        self.0
            .resource_transaction(|| async {
                checkpoint
                    .stage_checkpoint(BOOTSTRAP_STATE, 1, Some(&state))
                    .await
            })
            .await?;
        Ok(())
    }
}

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

/// A held provider lifecycle, not a sampled stop or a processing-state proof.
pub trait BootstrapRetirement: Send + Sync {
    /// Release only after confirmed configuration rejection.
    fn resume(self: Box<Self>);
    /// Permanently revoke the stopped provider. Unresolved drop must do the same.
    fn retire(self: Box<Self>);
}

/// A query-scoped snapshot provider. Dropping its stream must cancel its work;
/// the query awaits `stop` before releasing storage or deprovisioning its state.
#[async_trait]
pub trait ComputationBootstrapProvider: Send + Sync {
    /// Hold a positively stopped provider against reuse through configuration
    /// resolution. An opaque provider cannot infer this capability from `stop`.
    fn freeze_for_retirement(&self) -> anyhow::Result<Box<dyn BootstrapRetirement>> {
        anyhow::bail!("bootstrap provider does not support verified retirement")
    }
    /// Providers constructed for one query must reject other construction owners.
    fn validate_query_scope(
        &self,
        _instance_id: &str,
        _graph_id: &str,
        _component_id: &super::ComponentId,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    /// Declare a bound recovery reader, if used. The query validates the actual
    /// owner before preparation; equal graph/query names are not sufficient.
    fn recovery_reader(&self) -> Option<super::SourceProgressReader> {
        None
    }
    /// Finish asynchronous snapshot cleanup, including after cancelled startup.
    /// Retain unfinished workers on cancellation/error and reject another snapshot
    /// until cleanup succeeds. The default is for providers with no owned workers.
    async fn stop(&self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Establish source subscription readiness before deciding whether persisted
    /// query state can be reused. This must not consume snapshot records.
    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        Ok(BootstrapPreparation::Ready)
    }
    async fn prepare_with_state(
        &self,
        _state: &dyn BootstrapState,
    ) -> anyhow::Result<BootstrapPreparation> {
        self.prepare().await
    }
    /// Whether prepared source subscriptions supplied a snapshot for this start.
    /// Standalone snapshot providers always supply one; plugin adapters can
    /// distinguish an actual new snapshot from a checkpoint-based replay.
    fn has_pending_snapshot(&self) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot>;
    async fn snapshot_with_state(
        &self,
        _state: &dyn BootstrapState,
    ) -> anyhow::Result<ComputationBootstrapSnapshot> {
        self.snapshot().await
    }
    /// Boundaries that become available only after the snapshot stream closes.
    async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>> {
        Ok(Vec::new())
    }
    /// Optional final handover state, committed atomically with all source
    /// watermarks and the completed-bootstrap marker, never before them.
    fn completion_state(&self) -> anyhow::Result<Option<Bytes>> {
        Ok(None)
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
