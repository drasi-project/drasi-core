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

use std::sync::Arc;

use async_trait::async_trait;

use crate::interface::{CheckpointStore, IndexError, IndexSet, LiveResultsWriter, OutboxWriter};

use super::{
    AtomicResultTransaction, ComputationQueryError, Result, StorageDurability, TransactionDomain,
};

/// A constructed resource and its explicit, optional transaction participation.
/// This does not extend the legacy writer/plugin traits.
pub struct ComputationResource<T: ?Sized> {
    resource: Arc<T>,
    domain: Option<TransactionDomain>,
}

impl<T: ?Sized> ComputationResource<T> {
    pub fn independent(resource: Arc<T>) -> Self {
        Self {
            resource,
            domain: None,
        }
    }

    /// The provider asserts that this resource stages mutations in this domain.
    pub fn participating(resource: Arc<T>, domain: &TransactionDomain) -> Self {
        Self {
            resource,
            domain: Some(domain.clone()),
        }
    }

    pub fn resource(&self) -> &Arc<T> {
        &self.resource
    }

    fn participates_in(&self, domain: &TransactionDomain) -> bool {
        self.domain.as_ref() == Some(domain)
    }
}

/// One graph-owned stateful component's indexes and explicit output resources.
///
/// A query or [`super::ComputationTransaction`] consumes the bundle. Its index
/// fields cannot subsequently be replaced behind an already validated transaction.
pub struct ComputationIndexes {
    set: IndexSet,
    domain: Option<TransactionDomain>,
    checkpoint: Option<ComputationResource<dyn CheckpointStore>>,
    outbox: Option<ComputationResource<dyn OutboxWriter>>,
    live_results: Option<ComputationResource<dyn LiveResultsWriter>>,
    identity: Arc<()>,
    cleanup: Option<Arc<dyn ComputationResourceCleanup>>,
    durability: StorageDurability,
    pub(super) group_session: Option<Arc<super::transaction_group::GroupSession>>,
}

impl ComputationIndexes {
    pub fn try_new(
        set: IndexSet,
        domain: Option<TransactionDomain>,
        checkpoint: Option<ComputationResource<dyn CheckpointStore>>,
        outbox: Option<ComputationResource<dyn OutboxWriter>>,
        live_results: Option<ComputationResource<dyn LiveResultsWriter>>,
    ) -> Result<Self> {
        if domain
            .as_ref()
            .is_some_and(|domain| !domain.belongs_to(&set.session_control))
        {
            return Err(ComputationQueryError::TransactionMismatch);
        }
        Ok(Self {
            set,
            domain,
            checkpoint,
            outbox,
            live_results,
            identity: Arc::new(()),
            cleanup: None,
            durability: StorageDurability::UNKNOWN,
            group_session: None,
        })
    }

    /// Declare the commit boundary of this constructed bundle. This does not
    /// establish atomic participation; `atomic_result_transaction` checks that.
    pub fn with_durability(mut self, durability: StorageDurability) -> Self {
        self.durability = durability;
        self
    }

    pub fn durability(&self) -> StorageDurability {
        self.durability
    }

    pub(super) fn for_group_member(
        &self,
        session: Arc<super::transaction_group::GroupSession>,
        journal: Option<&str>,
    ) -> Result<Self> {
        self.atomic_result_transaction()?;
        let control: Arc<dyn crate::interface::SessionControl> = session.clone();
        let domain = TransactionDomain::new(control.clone());
        let mut resources = Self::try_new(
            IndexSet {
                element_index: self.set.element_index.clone(),
                archive_index: self.set.archive_index.clone(),
                result_index: self.set.result_index.clone(),
                future_queue: self.set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                self.checkpoint_store().expect("atomic checkpoint").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                Arc::new(super::group_outbox::GroupOutbox::new(
                    self.outbox_writer().expect("atomic outbox").clone(),
                    journal,
                )),
                &domain,
            )),
            Some(ComputationResource::participating(
                self.live_results_writer()
                    .expect("atomic projection")
                    .clone(),
                &domain,
            )),
        )?
        .with_cleanup(session.clone())
        .with_durability(self.durability);
        resources.group_session = Some(session);
        Ok(resources)
    }

    pub fn with_cleanup(mut self, cleanup: Arc<dyn ComputationResourceCleanup>) -> Self {
        self.cleanup = Some(cleanup);
        self
    }

    pub fn cleanup(&self) -> Option<&Arc<dyn ComputationResourceCleanup>> {
        self.cleanup.as_ref()
    }

    pub fn indexes(&self) -> &IndexSet {
        &self.set
    }

    pub fn checkpoint_store(&self) -> Option<&Arc<dyn CheckpointStore>> {
        self.checkpoint.as_ref().map(ComputationResource::resource)
    }
    /// Supply an independent fallback only when the provider has no checkpoint
    /// store. This cannot upgrade an incomplete bundle to an atomic transaction.
    pub fn with_fallback_checkpoint(mut self, store: Arc<dyn CheckpointStore>) -> Self {
        if self.checkpoint.is_none() {
            self.checkpoint = Some(ComputationResource::independent(store));
            self.durability = self.durability.intersection(StorageDurability::UNKNOWN);
        }
        self
    }

    pub fn outbox_writer(&self) -> Option<&Arc<dyn OutboxWriter>> {
        self.outbox.as_ref().map(ComputationResource::resource)
    }

    pub fn live_results_writer(&self) -> Option<&Arc<dyn LiveResultsWriter>> {
        self.live_results
            .as_ref()
            .map(ComputationResource::resource)
    }

    /// Validate all output resources and the actual core session as one scope.
    /// No partial bundle or independent writer is upgraded to atomic output.
    pub fn atomic_result_transaction(&self) -> Result<AtomicResultTransaction> {
        let Some(domain) = &self.domain else {
            return Err(ComputationQueryError::AtomicOutputUnsupported);
        };
        let complete = self
            .checkpoint
            .as_ref()
            .is_some_and(|resource| resource.participates_in(domain))
            && self
                .outbox
                .as_ref()
                .is_some_and(|resource| resource.participates_in(domain))
            && self
                .live_results
                .as_ref()
                .is_some_and(|resource| resource.participates_in(domain));
        if !complete {
            return Err(ComputationQueryError::AtomicOutputUnsupported);
        }
        Ok(AtomicResultTransaction {
            domain: domain.clone(),
            bundle: self.identity.clone(),
        })
    }
}

/// Explicit graph-only provider. A legacy plugin is never inferred to implement
/// this interface or to support atomic computation output.
#[async_trait]
pub trait ComputationIndexProvider: Send + Sync {
    /// Construct isolated resources for one component in the supplied graph scope.
    /// `query_id` is the historical parameter name; non-query stateful components
    /// also use their component ID here and exclusively own the returned bundle.
    async fn create_indexes(
        &self,
        graph_id: &str,
        query_id: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError>;

    fn is_volatile(&self) -> bool;

    /// The actual prerequisite of a namespace-only wrapper. This is a declared
    /// ownership link, never a matching path or a shared transaction guarantee.
    fn provider_dependency(&self) -> Option<&Arc<dyn ComputationIndexProvider>> {
        None
    }

    /// Actual shared-session owner, when the provider exposes grouped resources.
    fn transaction_group(&self) -> Option<&super::ComputationTransactionGroup> {
        None
    }

    fn durability(&self) -> StorageDurability {
        if self.is_volatile() {
            StorageDurability::VOLATILE
        } else {
            StorageDurability::UNKNOWN
        }
    }
}

/// Explicit ownership of provider work that can outlive a cancelled await.
/// Cancellation stops new submissions; shutdown must await registered work and
/// preserve cleanup responsibility if its own future is dropped.
#[async_trait]
pub trait ComputationResourceCleanup: Send + Sync {
    fn cancel(&self);
    async fn shutdown(&self) -> std::result::Result<(), IndexError>;
    /// Await already submitted work without sealing a healthy resource scope.
    async fn quiesce(&self) -> std::result::Result<(), IndexError> {
        Err(IndexError::NotSupported)
    }
}

/// Explicit volatile provider for the parallel graph; no durable/atomic claim.
#[derive(Default)]
pub struct InMemoryComputationProvider;

#[async_trait]
impl ComputationIndexProvider for InMemoryComputationProvider {
    async fn create_indexes(
        &self,
        _graph_id: &str,
        _query_id: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        use crate::in_memory_index::{
            in_memory_element_index::InMemoryElementIndex,
            in_memory_future_queue::InMemoryFutureQueue,
            in_memory_result_index::InMemoryResultIndex,
        };
        let elements = Arc::new(InMemoryElementIndex::new());
        ComputationIndexes::try_new(
            IndexSet {
                element_index: elements.clone(),
                archive_index: elements,
                result_index: Arc::new(InMemoryResultIndex::new()),
                future_queue: Arc::new(InMemoryFutureQueue::new()),
                session_control: Arc::new(crate::interface::NoOpSessionControl),
            },
            None,
            None,
            None,
            None,
        )
        .map(|indexes| indexes.with_durability(StorageDurability::VOLATILE))
        .map_err(IndexError::other)
    }
    fn is_volatile(&self) -> bool {
        true
    }
}
