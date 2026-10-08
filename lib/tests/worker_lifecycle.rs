// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_lib::{
    channels::ComponentStatus,
    context::workers::WorkerCleanupError,
    queries::base::QueryBase,
    reactions::common::base::{ReactionBase, ReactionBaseParams},
    sources::base::{SourceBase, SourceBaseParams},
    Query,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    sync::{oneshot, RwLock},
    task::JoinHandle,
};

enum Base {
    Source(SourceBase),
    Query(QueryBase),
    Reaction(ReactionBase),
}

impl Base {
    fn cases() -> [Self; 3] {
        [
            Self::Source(SourceBase::new(SourceBaseParams::new("source")).expect("source base")),
            Self::Query(
                QueryBase::new(Query::cypher("query").query("MATCH (n) RETURN n").build())
                    .expect("query base"),
            ),
            Self::Reaction(ReactionBase::new(ReactionBaseParams::new(
                "reaction",
                vec![],
            ))),
        ]
    }

    async fn install(&self, worker: JoinHandle<()>) {
        match self {
            Self::Source(base) => base.set_task_handle(worker).await,
            Self::Query(base) => base.set_task_handle(worker).await,
            Self::Reaction(base) => base.set_processing_task(worker).await,
        }
    }

    fn slot(&self) -> &RwLock<Option<JoinHandle<()>>> {
        match self {
            Self::Source(base) => &base.task_handle,
            Self::Query(base) => &base.task_handle,
            Self::Reaction(base) => &base.processing_task,
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        match self {
            Self::Source(base) => base.stop_common().await,
            Self::Query(base) => base.stop_common().await,
            Self::Reaction(base) => base.stop_common().await,
        }
    }

    async fn set_running(&self) {
        match self {
            Self::Source(base) => base.set_status(ComponentStatus::Running, None).await,
            Self::Query(base) => base.set_status(ComponentStatus::Running, None).await,
            Self::Reaction(base) => base.set_status(ComponentStatus::Running, None).await,
        }
    }

    async fn status(&self) -> ComponentStatus {
        match self {
            Self::Source(base) => base.get_status().await,
            Self::Query(base) => base.get_status().await,
            Self::Reaction(base) => base.get_status().await,
        }
    }
}

struct Exit(Arc<AtomicUsize>);

impl Drop for Exit {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn interrupted_stop() {
    for base in Base::cases() {
        let exited = Arc::new(AtomicUsize::new(0));
        let (started, ready) = oneshot::channel();
        let (release, blocked) = oneshot::channel();
        base.install(tokio::spawn({
            let exited = exited.clone();
            async move {
                let _exit = Exit(exited);
                started.send(()).expect("worker readiness observed");
                blocked.await.expect("worker released");
            }
        }))
        .await;
        ready.await.expect("worker started");
        base.set_running().await;
        {
            let stop = base.stop();
            tokio::pin!(stop);
            assert!(futures::poll!(&mut stop).is_pending());
        }
        assert!(base.slot().read().await.is_some());
        assert_ne!(base.status().await, ComponentStatus::Stopped);
        assert_eq!(exited.load(Ordering::SeqCst), 0);

        release.send(()).expect("worker still owned");
        base.stop().await.expect("retried cleanup");
        assert_eq!(exited.load(Ordering::SeqCst), 1);
        assert!(base.slot().read().await.is_none());
        assert_eq!(base.status().await, ComponentStatus::Stopped);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_stop_preserves_owned_workers_current_thread() {
    interrupted_stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_stop_preserves_owned_workers_multi_thread() {
    interrupted_stop().await;
}

#[tokio::test]
async fn stop_timeout_is_not_completed_cleanup() {
    for base in Base::cases() {
        let exited = Arc::new(AtomicUsize::new(0));
        let (started, ready) = oneshot::channel();
        base.install(tokio::spawn({
            let exited = exited.clone();
            async move {
                let _exit = Exit(exited);
                started.send(()).expect("worker readiness observed");
                std::future::pending::<()>().await;
            }
        }))
        .await;
        ready.await.expect("worker started");
        base.set_running().await;
        let error = base.stop().await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<WorkerCleanupError>(),
            Some(WorkerCleanupError::TimedOut { .. })
        ));
        assert!(base.slot().read().await.is_some());
        assert_ne!(base.status().await, ComponentStatus::Stopped);

        base.stop().await.unwrap();
        assert_eq!(exited.load(Ordering::SeqCst), 1);
        assert!(base.slot().read().await.is_none());
        assert_eq!(base.status().await, ComponentStatus::Stopped);
    }
}

#[tokio::test]
async fn worker_panic_is_typed_and_cleanup_can_be_retried() {
    for base in Base::cases() {
        base.set_running().await;
        base.install(tokio::spawn(async { panic!("injected worker failure") }))
            .await;
        let error = base.stop().await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<WorkerCleanupError>(),
            Some(WorkerCleanupError::Join(error)) if error.is_panic()
        ));
        assert!(base.slot().read().await.is_none());
        assert_ne!(base.status().await, ComponentStatus::Stopped);
        base.stop().await.unwrap();
        assert_eq!(base.status().await, ComponentStatus::Stopped);
    }
}

async fn joined_forwarders() {
    let base = ReactionBase::new(ReactionBaseParams::new("forwarders", vec![]));
    let exited = Arc::new(AtomicUsize::new(0));
    for _ in 0..8 {
        let (started, ready) = oneshot::channel();
        base.subscription_tasks.write().await.push(tokio::spawn({
            let exited = exited.clone();
            async move {
                let _exit = Exit(exited);
                started.send(()).expect("forwarder readiness observed");
                std::future::pending::<()>().await;
            }
        }));
        ready.await.expect("forwarder started");
    }
    base.stop_common().await.expect("forwarders joined");
    assert_eq!(exited.load(Ordering::SeqCst), 8);
    assert!(base.subscription_tasks.read().await.is_empty());
    assert_eq!(base.get_status().await, ComponentStatus::Stopped);
}

#[tokio::test(flavor = "current_thread")]
async fn reaction_stop_joins_forwarders_current_thread() {
    joined_forwarders().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaction_stop_joins_forwarders_multi_thread() {
    joined_forwarders().await;
}
