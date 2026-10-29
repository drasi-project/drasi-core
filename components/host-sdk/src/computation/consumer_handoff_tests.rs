// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{ComputationIndexes, ComputationResource, TransactionDomain},
    interface::{IndexError, IndexSet, SessionControl},
};

struct Boundary {
    remaining: AtomicUsize,
    after: bool,
    cancel: bool,
    entered: tokio::sync::Notify,
}
struct FaultControl {
    inner: Arc<dyn SessionControl>,
    boundary: Arc<Boundary>,
}
#[async_trait]
impl SessionControl for FaultControl {
    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), IndexError> {
        let hit = self.boundary.remaining.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |remaining| remaining.checked_sub(1),
        ) == Ok(1);
        if !hit || self.boundary.after {
            self.inner.commit().await?;
        }
        if hit {
            self.boundary.entered.notify_one();
            if self.boundary.cancel {
                std::future::pending::<()>().await;
            }
            return Err(IndexError::IOError);
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.inner.rollback()
    }
}
struct FaultProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    boundary: Arc<Boundary>,
}
#[async_trait]
impl ComputationIndexProvider for FaultProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, component).await?;
        let control: Arc<dyn SessionControl> = Arc::new(FaultControl {
            inner: original.indexes().session_control.clone(),
            boundary: self.boundary.clone(),
        });
        let domain = TransactionDomain::new(control.clone());
        let set = original.indexes();
        Ok(ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original.checkpoint_store().unwrap().clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().unwrap().clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.live_results_writer().unwrap().clone(),
                &domain,
            )),
        )
        .unwrap()
        .with_cleanup(original.cleanup().unwrap().clone())
        .with_durability(original.durability()))
    }
    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "required child of native_consumer_completion_recovers_after_process_exit"]
async fn native_consumer_completion_crash_worker() -> anyhow::Result<()> {
    let directory = std::path::PathBuf::from(std::env::var("DRASI_CONSUMER_CRASH_DIRECTORY")?);
    let mode = match std::env::var("DRASI_CONSUMER_CRASH_MODE")?.as_str() {
        "external" => sdk::ConsumerMode::External,
        "transactional" => sdk::ConsumerMode::Transactional,
        _ => anyhow::bail!("unknown crash mode"),
    };
    let after: bool = std::env::var("DRASI_CONSUMER_CRASH_AFTER")?.parse()?;
    let plugin = super::super::super::factory::recovery_tests::sqlite_plugin()?;
    let boundary = Arc::new(Boundary {
        remaining: AtomicUsize::new(0),
        after,
        cancel: true,
        entered: tokio::sync::Notify::new(),
    });
    let mut consumer = factory(&plugin, mode)
        .create_consumer(
            id("consumer"),
            json!({"path":directory.join("effects.db")}),
            scope(),
            Arc::new(FaultProvider {
                inner: provider(&directory.join("progress"))?,
                boundary: boundary.clone(),
            }),
            options(),
        )
        .await?;
    consumer.start().await?;
    boundary.remaining.store(2, Ordering::Release);
    let operation = consumer.handle(batch()?);
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => anyhow::bail!("completion did not pause: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), boundary.entered.notified()) => {
            result?;
            std::process::exit(if after { 87 } else { 86 });
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_completion_recovers_after_process_exit() -> anyhow::Result<()> {
    let plugin = super::super::super::factory::recovery_tests::sqlite_plugin()?;
    for mode in [
        sdk::ConsumerMode::External,
        sdk::ConsumerMode::Transactional,
    ] {
        for after in [false, true] {
            let directory = tempfile::tempdir()?;
            let mut child = tokio::process::Command::new(std::env::current_exe()?);
            child
                .args([
                    "--exact",
                    concat!(module_path!(), "::native_consumer_completion_crash_worker")
                        .strip_prefix("drasi_host_sdk::")
                        .unwrap(),
                    "--ignored",
                ])
                .env("DRASI_CONSUMER_CRASH_DIRECTORY", directory.path())
                .env(
                    "DRASI_CONSUMER_CRASH_MODE",
                    if mode == sdk::ConsumerMode::External {
                        "external"
                    } else {
                        "transactional"
                    },
                )
                .env("DRASI_CONSUMER_CRASH_AFTER", after.to_string())
                .kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(30), child.output()).await??;
            assert_eq!(
                output.status.code(),
                Some(if after { 87 } else { 86 }),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let provider = provider(&directory.path().join("progress"))?;
            if mode == sdk::ConsumerMode::Transactional {
                assert_eq!(
                    count(provider.as_ref()).await?,
                    after.then_some(ElementValue::Integer(1))
                );
            } else {
                assert_eq!(effects(&directory.path().join("effects.db"))?, (1, 1, 1));
            }
            let mut consumer = factory(&plugin, mode)
                .create_consumer(
                    id("consumer"),
                    json!({"path":directory.path().join("effects.db")}),
                    sdk::Scope {
                        generation: 2,
                        ..scope()
                    },
                    provider.clone(),
                    options(),
                )
                .await?;
            consumer.start().await?;
            assert_eq!(
                consumer.progress().await?[0].completed_operations,
                usize::from(after)
            );
            consumer.handle(batch()?).await?;
            assert_eq!(consumer.progress().await?[0].handled_sequence, 1);
            consumer.handle(batch()?).await?;
            consumer.stop().await?;
            if mode == sdk::ConsumerMode::Transactional {
                assert_eq!(
                    count(provider.as_ref()).await?,
                    Some(ElementValue::Integer(3))
                );
            } else {
                assert_eq!(
                    effects(&directory.path().join("effects.db"))?,
                    (3, if after { 3 } else { 4 }, 3)
                );
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_real_library_recovers_uncertain_completion_without_losing_operations(
) -> anyhow::Result<()> {
    let plugin = super::super::super::factory::recovery_tests::sqlite_plugin()?;
    for mode in [
        sdk::ConsumerMode::External,
        sdk::ConsumerMode::Transactional,
    ] {
        for after in [false, true] {
            for cancel in [false, true] {
                let directory = tempfile::tempdir()?;
                let path = directory.path().join("effects.db");
                let inner = provider(&directory.path().join("progress"))?;
                let boundary = Arc::new(Boundary {
                    remaining: AtomicUsize::new(0),
                    after,
                    cancel,
                    entered: tokio::sync::Notify::new(),
                });
                let provider = Arc::new(FaultProvider {
                    inner: inner.clone(),
                    boundary: boundary.clone(),
                });
                let factory = factory(&plugin, mode);
                let mut consumer = factory
                    .create_consumer(
                        id("consumer"),
                        json!({"path":path}),
                        scope(),
                        provider,
                        options(),
                    )
                    .await?;
                consumer.start().await?;
                // After startup, admission commits first, then operation completion.
                boundary.remaining.store(2, Ordering::Release);
                if cancel {
                    let handle = consumer.handle(batch()?);
                    tokio::pin!(handle);
                    tokio::select! {
                        result = &mut handle => panic!("commit boundary unexpectedly completed: {result:?}"),
                        entered = tokio::time::timeout(Duration::from_secs(10), boundary.entered.notified()) => { entered?; }
                    }
                } else {
                    assert!(consumer.handle(batch()?).await.is_err());
                }
                assert_eq!(boundary.remaining.load(Ordering::Acquire), 0);
                assert!(
                    consumer.progress().await.is_err(),
                    "uncertain commit must fence cached progress"
                );
                assert!(consumer.handle(batch()?).await.is_err());
                consumer.stop().await?;
                if mode == sdk::ConsumerMode::Transactional {
                    assert_eq!(
                        count(inner.as_ref()).await?,
                        after.then_some(ElementValue::Integer(1))
                    );
                } else {
                    assert_eq!(effects(&path)?, (1, 1, 1));
                }
                let mut consumer = factory
                    .create_consumer(
                        id("consumer"),
                        json!({"path":path}),
                        scope(),
                        inner.clone(),
                        options(),
                    )
                    .await?;
                consumer.start().await?;
                assert_eq!(
                    consumer.progress().await?[0].completed_operations,
                    usize::from(after)
                );
                consumer.handle(batch()?).await?;
                assert_eq!(consumer.progress().await?[0].handled_sequence, 1);
                consumer.stop().await?;
                if mode == sdk::ConsumerMode::Transactional {
                    assert_eq!(count(inner.as_ref()).await?, Some(ElementValue::Integer(3)));
                } else {
                    assert_eq!(effects(&path)?, (3, if after { 3 } else { 4 }, 3));
                }
            }
        }
    }
    Ok(())
}
