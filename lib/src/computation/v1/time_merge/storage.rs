// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::{future::Future, pin::Pin};
use tokio::sync::Mutex;

type IndexConstruction = Pin<
    Box<dyn Future<Output = anyhow::Result<drasi_core::computation::ComputationIndexes>> + Send>,
>;

#[derive(Default)]
struct StorageState {
    opening: Option<IndexConstruction>,
    transaction: Option<Arc<ComputationTransaction>>,
}

impl StorageState {
    async fn settle(&mut self) -> anyhow::Result<()> {
        if let Some(opening) = self.opening.as_mut() {
            let indexes = opening.await;
            self.opening = None;
            // Retain cleanup before validating the extension-supplied bundle.
            self.transaction = Some(Arc::new(ComputationTransaction::for_processing(indexes?)));
        }
        Ok(())
    }
}

pub(super) struct SourceTimeMergeStorageLease(Arc<SourceTimeMergeStorage>);

impl Drop for SourceTimeMergeStorageLease {
    fn drop(&mut self) {
        self.0.leased.store(false, Ordering::Release);
    }
}

/// Graph-owned persistent storage for one source-time merger.
///
/// The actual index provider is a separate declared resource dependency. This
/// owner retains an acquired transaction even when component construction fails
/// or is cancelled; graph resource cleanup must complete before replacement.
pub struct SourceTimeMergeStorage {
    instance: Arc<str>,
    graph: Arc<str>,
    component: ComponentId,
    provider: Arc<dyn ComputationIndexProvider>,
    state: Mutex<StorageState>,
    closed: AtomicBool,
    leased: AtomicBool,
}

impl SourceTimeMergeStorage {
    /// Create an unopened owner. No provider work starts until the graph
    /// constructs the merger. Volatile and shared-transaction providers are not
    /// compatible with the merger's independent persistent transaction.
    pub fn new(
        instance: impl Into<Arc<str>>,
        graph: impl Into<Arc<str>>,
        component: ComponentId,
        provider: Arc<dyn ComputationIndexProvider>,
    ) -> anyhow::Result<Arc<Self>> {
        let instance = instance.into();
        let graph = graph.into();
        data::validate_identifier("merge instance", &instance)?;
        data::validate_identifier("merge graph", &graph)?;
        anyhow::ensure!(
            !provider.is_volatile() && provider.transaction_group().is_none(),
            "time merge requires its own persistent transaction owner"
        );
        Ok(Arc::new(Self {
            instance,
            graph,
            component,
            provider,
            state: Mutex::new(StorageState::default()),
            closed: AtomicBool::new(false),
            leased: AtomicBool::new(false),
        }))
    }

    /// Bind with graph ownership and retain the actual provider as a resource
    /// dependency. Resource names do not prove transaction or storage ownership.
    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(ResourceRole::IndexBackend, self.clone()).with_cleanup(self.clone())
    }

    pub(super) fn validate_owner(
        &self,
        instance: Option<&str>,
        graph: &str,
        component: &ComponentId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            instance.is_none_or(|instance| instance == self.instance.as_ref())
                && graph == self.graph.as_ref()
                && component == &self.component,
            "time merge storage belongs to another instance, graph or component"
        );
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "time merge storage has been retired"
        );
        Ok(())
    }

    pub(super) fn provider(&self) -> &Arc<dyn ComputationIndexProvider> {
        &self.provider
    }

    fn claim(self: &Arc<Self>) -> anyhow::Result<SourceTimeMergeStorageLease> {
        anyhow::ensure!(
            self.leased
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "time merge storage is already owned by a constructed component"
        );
        Ok(SourceTimeMergeStorageLease(self.clone()))
    }

    pub(super) async fn open(
        self: &Arc<Self>,
    ) -> anyhow::Result<(Arc<ComputationTransaction>, SourceTimeMergeStorageLease)> {
        let lease = self.claim()?;
        let mut state = self.state.lock().await;
        self.validate_owner(Some(&self.instance), &self.graph, &self.component)?;
        if state.transaction.is_none() && state.opening.is_none() {
            let scope = format!(
                "source-time-merge/{}/{}/{}",
                self.instance.len(),
                self.instance,
                self.graph
            );
            let provider = self.provider.clone();
            let component = self.component.clone();
            state.opening = Some(Box::pin(async move {
                Ok(provider.create_indexes(&scope, component.as_str()).await?)
            }));
        }
        state.settle().await?;
        self.validate_owner(Some(&self.instance), &self.graph, &self.component)?;
        let owner = state
            .transaction
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("time merge storage was not acquired"))?;
        anyhow::ensure!(
            owner.resources().cleanup().is_some()
                && owner
                    .resources()
                    .checkpoint_store()
                    .is_some_and(|store| store.is_persistent()),
            "time merge requires persistent checkpoints and asynchronous cleanup"
        );
        owner.resources().atomic_result_transaction()?;
        anyhow::ensure!(
            !owner.recovery_required(),
            "time merge storage requires cleanup and reconstruction"
        );
        Ok((owner.clone(), lease))
    }
}

#[async_trait]
impl ResourceCleanup for SourceTimeMergeStorage {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::Release);
        let mut state = self.state.lock().await;
        state.settle().await?;
        if let Some(owner) = state.transaction.as_ref() {
            owner.shutdown().await?;
        }
        state.transaction.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::{
        computation::{
            ComputationIndexes, ComputationResourceCleanup, InMemoryComputationProvider,
        },
        interface::IndexError,
    };
    use std::{result::Result, sync::atomic::AtomicUsize};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct Provider {
        entered: Notify,
        release: Notify,
        opens: AtomicUsize,
        cleanups: Arc<AtomicUsize>,
    }

    struct Cleanup(Arc<AtomicUsize>);

    #[async_trait]
    impl ComputationResourceCleanup for Cleanup {
        fn cancel(&self) {}
        async fn shutdown(&self) -> Result<(), IndexError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl ComputationIndexProvider for Provider {
        async fn create_indexes(
            &self,
            graph: &str,
            component: &str,
        ) -> Result<ComputationIndexes, IndexError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            Ok(InMemoryComputationProvider
                .create_indexes(graph, component)
                .await?
                .with_cleanup(Arc::new(Cleanup(self.cleanups.clone()))))
        }
        fn is_volatile(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn cancelled_acquisition_and_cleanup_retain_the_original_provider_future(
    ) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let provider = Arc::new(Provider::default());
            let storage = SourceTimeMergeStorage::new(
                "instance",
                "graph",
                ComponentId::try_new("merge")?,
                provider.clone(),
            )?;
            let mut opening = Box::pin(storage.open());
            tokio::select! {
                _ = provider.entered.notified() => {}
                _ = &mut opening => anyhow::bail!("provider must remain blocked"),
            }
            drop(opening);
            let mut cleanup = Box::pin(storage.shutdown());
            assert!(futures::poll!(&mut cleanup).is_pending());
            drop(cleanup);
            provider.release.notify_one();
            storage.shutdown().await?;
            assert_eq!(provider.opens.load(Ordering::SeqCst), 1);
            assert_eq!(provider.cleanups.load(Ordering::SeqCst), 1);
            assert!(storage.open().await.is_err(), "retired owner cannot reopen");
            Ok(())
        })
        .await?
    }

    #[tokio::test]
    async fn actual_storage_lease_rejects_aliasing_and_retains_invalid_acquisitions(
    ) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let provider = Arc::new(Provider::default());
            let storage = SourceTimeMergeStorage::new(
                "instance",
                "graph",
                ComponentId::try_new("merge")?,
                provider.clone(),
            )?;
            let lease = storage.claim()?;
            assert!(
                storage.open().await.is_err(),
                "matching names do not prove exclusive ownership"
            );
            assert_eq!(provider.opens.load(Ordering::SeqCst), 0);
            drop(lease);
            provider.release.notify_one();
            assert!(
                storage.open().await.is_err(),
                "actual bundle is not persistent/atomic"
            );
            assert_eq!(provider.opens.load(Ordering::SeqCst), 1);
            assert_eq!(provider.cleanups.load(Ordering::SeqCst), 0);
            assert!(storage.open().await.is_err());
            assert_eq!(
                provider.opens.load(Ordering::SeqCst),
                1,
                "retry must not orphan the first bundle"
            );
            storage.shutdown().await?;
            assert_eq!(provider.cleanups.load(Ordering::SeqCst), 1);
            Ok(())
        })
        .await?
    }
}
