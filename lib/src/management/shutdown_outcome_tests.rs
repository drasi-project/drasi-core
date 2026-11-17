// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::super::*;
use crate::{DrasiLib, ShutdownCompletion};
use std::sync::{atomic::AtomicUsize, Mutex as StdMutex, Weak};
use tokio::sync::{Notify, Semaphore};

#[derive(Default)]
struct StoreControl {
    closes: AtomicUsize,
    released: AtomicUsize,
    dropped: AtomicUsize,
    fail_close: AtomicBool,
    panic_close: AtomicBool,
    panic_after_close: AtomicBool,
    hold_close: AtomicBool,
    release_close: Notify,
    session: StdMutex<Weak<ProbeSession>>,
}

#[derive(Default)]
struct ProbeStore {
    inner: crate::management::memory::MemoryConfigStore,
    control: Arc<StoreControl>,
}

#[async_trait::async_trait]
impl ConfigurationStore for ProbeStore {
    fn guarantees(&self) -> ConfigurationStoreGuarantees {
        self.inner.guarantees()
    }

    async fn open(&self, instance_id: &str) -> Result<Arc<dyn ConfigurationSession>> {
        let session = Arc::new(ProbeSession {
            inner: self.inner.open(instance_id).await?,
            control: self.control.clone(),
        });
        *self.control.session.lock().unwrap() = Arc::downgrade(&session);
        Ok(session)
    }
}

struct ProbeSession {
    inner: Arc<dyn ConfigurationSession>,
    control: Arc<StoreControl>,
}

#[derive(Debug, thiserror::Error)]
#[error("configuration close needs retry")]
struct StoreCloseFailure;

#[async_trait::async_trait]
impl ConfigurationSession for ProbeSession {
    async fn load(&self) -> Result<CommittedConfiguration> {
        self.inner.load().await
    }

    async fn new_request_id(&self) -> Result<String> {
        self.inner.new_request_id().await
    }

    async fn commit(
        &self,
        expected_revision: u64,
        request_id: &str,
        desired: &DesiredInstance,
        creation: Option<&AcceptedComponentCreation>,
        updates: &[AcceptedComponentUpdate],
    ) -> Result<AcceptanceReceipt> {
        self.inner
            .commit(expected_revision, request_id, desired, creation, updates)
            .await
    }

    async fn complete_updates(
        &self,
        expected_revision: u64,
        updates: &[ComponentUpdateCompletion],
    ) -> Result<CommittedConfiguration> {
        self.inner
            .complete_updates(expected_revision, updates)
            .await
    }

    async fn receipt(&self, request_id: &str) -> Result<Option<AcceptanceReceipt>> {
        self.inner.receipt(request_id).await
    }

    async fn snapshot(&self, name: &str) -> Result<CommittedConfiguration> {
        self.inner.snapshot(name).await
    }

    async fn load_snapshot(&self, name: &str) -> Result<Option<CommittedConfiguration>> {
        self.inner.load_snapshot(name).await
    }

    async fn list_snapshots(
        &self,
        after: Option<&str>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ConfigurationSnapshotSummary>> {
        self.inner.list_snapshots(after, limit).await
    }

    async fn delete_snapshot(&self, name: &str, expected_revision: u64) -> Result<bool> {
        self.inner.delete_snapshot(name, expected_revision).await
    }

    async fn close(&self) -> Result<()> {
        self.control.closes.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.control.panic_close.load(Ordering::SeqCst),
            "injected panic before close acknowledgement"
        );
        if self.control.fail_close.load(Ordering::SeqCst) {
            return Err(StoreCloseFailure.into());
        }
        if self.control.hold_close.load(Ordering::SeqCst) {
            self.control.release_close.notified().await;
        }
        self.inner.close().await?;
        self.control.released.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for ProbeSession {
    fn drop(&mut self) {
        self.control.dropped.fetch_add(1, Ordering::SeqCst);
        // Driver::run has sent the real successful close reply before its owned
        // session is destroyed. No synthetic close loop or result is substituted.
        assert!(
            !self.control.panic_after_close.load(Ordering::SeqCst),
            "injected management driver panic after store close"
        );
    }
}

fn builder(store: &Arc<ProbeStore>) -> crate::DrasiLibBuilder {
    DrasiLib::builder()
        .with_id("shutdown-outcome")
        .with_configuration_store(store.clone())
        .with_shutdown_timeout_secs(1)
        .with_cleanup_timeout_secs(30)
}

fn assert_join_panic(error: &crate::DrasiError) {
    assert!(error
        .downcast_ref::<tokio::task::JoinError>()
        .expect("preserve the actual driver's typed join failure")
        .is_panic());
}

fn assert_timeout(error: &crate::DrasiError) {
    assert!(matches!(
        error.downcast_ref::<GraphError>(),
        Some(GraphError::LifecycleTimeout(timeout)) if timeout.accepted
    ));
}

async fn successful_shutdown() -> Result<()> {
    let store = Arc::new(ProbeStore::default());
    let core = builder(&store).build().await?;
    core.start().await?;
    for _ in 0..2 {
        let outcome: crate::ShutdownOutcome = core.shutdown_with_outcome().await;
        assert!(matches!(outcome.completion, ShutdownCompletion::Complete));
        outcome.result?;
    }
    core.shutdown().await?;
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.released.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.dropped.load(Ordering::SeqCst), 1);
    assert!(store.control.session.lock().unwrap().upgrade().is_none());
    store.inner.open("shutdown-outcome").await?.close().await?;
    Ok(())
}

async fn acknowledged_driver_panic() -> Result<()> {
    let store = Arc::new(ProbeStore::default());
    store
        .control
        .panic_after_close
        .store(true, Ordering::SeqCst);
    let core = builder(&store).build().await?;
    for _ in 0..2 {
        let outcome = core.shutdown_with_outcome().await;
        assert!(matches!(outcome.completion, ShutdownCompletion::Complete));
        assert_join_panic(&outcome.result.unwrap_err());
    }
    assert_join_panic(&core.shutdown().await.unwrap_err());
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.released.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.dropped.load(Ordering::SeqCst), 1);
    assert!(store.control.session.lock().unwrap().upgrade().is_none());
    store.inner.open("shutdown-outcome").await?.close().await?;
    Ok(())
}

async fn missing_close_acknowledgement() -> Result<()> {
    let store = Arc::new(ProbeStore::default());
    store.control.panic_close.store(true, Ordering::SeqCst);
    let core = builder(&store).build().await?;
    for _ in 0..2 {
        let outcome = core.shutdown_with_outcome().await;
        assert!(matches!(outcome.completion, ShutdownCompletion::Incomplete));
        assert_join_panic(&outcome.result.unwrap_err());
    }
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.released.load(Ordering::SeqCst), 0);
    let reopened = store.inner.open("shutdown-outcome").await;
    assert!(matches!(
        reopened
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<ManagementError>()),
        Some(ManagementError::AlreadyOwned(_))
    ));
    Ok(())
}

async fn failed_close_acknowledgement() -> Result<()> {
    let store = Arc::new(ProbeStore::default());
    store.control.fail_close.store(true, Ordering::SeqCst);
    let core = builder(&store).build().await?;
    let outcome = core.shutdown_with_outcome().await;
    assert!(matches!(outcome.completion, ShutdownCompletion::Incomplete));
    assert!(outcome
        .result
        .unwrap_err()
        .downcast_ref::<StoreCloseFailure>()
        .is_some());
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.released.load(Ordering::SeqCst), 0);
    assert!(store.control.session.lock().unwrap().upgrade().is_some());
    assert!(!core
        .management
        .get()
        .unwrap()
        .task
        .lock()
        .await
        .driver
        .is_finished());
    store.control.fail_close.store(false, Ordering::SeqCst);
    for _ in 0..2 {
        let outcome = core.shutdown_with_outcome().await;
        assert!(matches!(outcome.completion, ShutdownCompletion::Complete));
        outcome.result?;
    }
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 2);
    assert_eq!(store.control.released.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.dropped.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn pending_close_acknowledgement() -> Result<()> {
    let store = Arc::new(ProbeStore::default());
    store.control.hold_close.store(true, Ordering::SeqCst);
    let core = builder(&store).build().await?;
    let outcome = core.shutdown_with_outcome().await;
    assert!(matches!(outcome.completion, ShutdownCompletion::Incomplete));
    assert_timeout(&outcome.result.unwrap_err());
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.released.load(Ordering::SeqCst), 0);
    assert!(store.control.session.lock().unwrap().upgrade().is_some());
    store.control.release_close.notify_one();
    for _ in 0..2 {
        let outcome = core.shutdown_with_outcome().await;
        assert!(matches!(outcome.completion, ShutdownCompletion::Complete));
        outcome.result?;
    }
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.released.load(Ordering::SeqCst), 1);
    Ok(())
}

struct HeldResource {
    calls: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl ResourceCleanup for HeldResource {
    async fn shutdown(&self) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.release.acquire().await?.forget();
        Ok(())
    }
}

fn resource_batch(resource: Arc<HeldResource>) -> GraphResult<ComponentBatch> {
    let id = ResourceId::try_new("held-shutdown-resource")?;
    ComponentBatch::builder()
        .declare_resource(ResourceSpecification {
            id: id.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "held-shutdown-resource".into(),
        })?
        .provide_resource(
            id,
            ResourceHandle::new(ResourceRole::StateStore, resource.clone()).with_cleanup(resource),
        )?
        .build()
}

async fn pending_resource_cleanup(adopted: bool) -> Result<()> {
    let store = Arc::new(ProbeStore::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let resource = Arc::new(HeldResource {
        calls: calls.clone(),
        release: release.clone(),
    });
    let weak = Arc::downgrade(&resource);
    let batch = resource_batch(resource)?;
    let core = if adopted {
        builder(&store).with_components(batch).build().await?
    } else {
        let core = builder(&store).build().await?;
        core.instance_graph.retain(batch, None);
        core
    };
    let outcome = core.shutdown_with_outcome().await;
    assert!(matches!(outcome.completion, ShutdownCompletion::Incomplete));
    outcome
        .result
        .expect_err("the held resource still owns cleanup");
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(weak.upgrade().is_some());
    release.add_permits(1);
    for _ in 0..2 {
        let outcome = core.shutdown_with_outcome().await;
        assert!(matches!(outcome.completion, ShutdownCompletion::Complete));
        outcome.result?;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    assert!(weak.upgrade().is_none());
    Ok(())
}

async fn pending_startup_worker() -> Result<()> {
    let store = Arc::new(ProbeStore::default());
    let core = builder(&store).build().await?;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(AtomicUsize::new(0));
    core.instance_graph.spawn_startup({
        let entered = entered.clone();
        let release = release.clone();
        let completed = completed.clone();
        async move {
            let _completion = super::Completion(completed);
            entered.notify_one();
            release.notified().await;
        }
    })?;
    entered.notified().await;
    let outcome = core.shutdown_with_outcome().await;
    assert!(matches!(outcome.completion, ShutdownCompletion::Incomplete));
    assert_timeout(&outcome.result.unwrap_err());
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 0);
    release.notify_one();
    for _ in 0..2 {
        let outcome = core.shutdown_with_outcome().await;
        assert!(matches!(outcome.completion, ShutdownCompletion::Complete));
        outcome.result?;
    }
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn joined_startup_worker_failure() -> Result<()> {
    let store = Arc::new(ProbeStore::default());
    let core = builder(&store).build().await?;
    let completed = Arc::new(AtomicUsize::new(0));
    core.instance_graph.spawn_startup({
        let completed = completed.clone();
        async move {
            let _completion = super::Completion(completed);
            panic!("injected startup worker panic");
        }
    })?;
    let outcome = core.shutdown_with_outcome().await;
    assert!(matches!(outcome.completion, ShutdownCompletion::Complete));
    assert_join_panic(&outcome.result.unwrap_err());
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    let outcome = core.shutdown_with_outcome().await;
    assert!(matches!(outcome.completion, ShutdownCompletion::Complete));
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert_eq!(store.control.closes.load(Ordering::SeqCst), 1);
    Ok(())
}

macro_rules! shutdown_outcome_flavors {
    ($current:ident, $multi:ident, $body:expr) => {
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn $current() -> Result<()> {
            $body.await
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $multi() -> Result<()> {
            tokio::time::timeout(std::time::Duration::from_secs(10), $body).await?
        }
    };
}

shutdown_outcome_flavors!(
    shutdown_outcome_success_current_thread,
    shutdown_outcome_success_multi_thread,
    successful_shutdown()
);
shutdown_outcome_flavors!(
    shutdown_outcome_acknowledged_driver_panic_current_thread,
    shutdown_outcome_acknowledged_driver_panic_multi_thread,
    acknowledged_driver_panic()
);
shutdown_outcome_flavors!(
    shutdown_outcome_missing_ack_current_thread,
    shutdown_outcome_missing_ack_multi_thread,
    missing_close_acknowledgement()
);
shutdown_outcome_flavors!(
    shutdown_outcome_failed_ack_current_thread,
    shutdown_outcome_failed_ack_multi_thread,
    failed_close_acknowledgement()
);
shutdown_outcome_flavors!(
    shutdown_outcome_pending_ack_current_thread,
    shutdown_outcome_pending_ack_multi_thread,
    pending_close_acknowledgement()
);
shutdown_outcome_flavors!(
    shutdown_outcome_graph_resource_current_thread,
    shutdown_outcome_graph_resource_multi_thread,
    pending_resource_cleanup(true)
);
shutdown_outcome_flavors!(
    shutdown_outcome_pending_batch_current_thread,
    shutdown_outcome_pending_batch_multi_thread,
    pending_resource_cleanup(false)
);
shutdown_outcome_flavors!(
    shutdown_outcome_pending_worker_current_thread,
    shutdown_outcome_pending_worker_multi_thread,
    pending_startup_worker()
);
shutdown_outcome_flavors!(
    shutdown_outcome_joined_worker_error_current_thread,
    shutdown_outcome_joined_worker_error_multi_thread,
    joined_startup_worker_failure()
);
