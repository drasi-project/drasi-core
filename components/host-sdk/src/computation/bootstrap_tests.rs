// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use bytes::Bytes;
use drasi_core::interface::StorageDurability;
use std::sync::Mutex;
use tokio_stream::StreamExt;

fn scope() -> sdk::Scope {
    sdk::Scope {
        instance_id: "native-bootstrap".into(),
        graph_id: "native-bootstrap".into(),
        generation: 1,
    }
}
fn progress() -> Arc<QuerySourceProgress> {
    Arc::new(
        QuerySourceProgress::new(
            "native-bootstrap",
            ComponentId::try_new("6ddf3eb2-187f-5740-a6ba-a07f98257ca8").unwrap(),
        )
        .unwrap(),
    )
}

fn bootstrap_plugin() -> Arc<NativePlugin> {
    unsafe {
        NativePlugin::from_entry_points_with_bootstrap(
            drasi_computation_plugin_metadata,
            drasi_computation_plugin_entry,
            None,
            None,
            Some(drasi_computation_plugin_bootstrap_v1),
        )
    }
    .unwrap()
}
fn provider(config: Value) -> Arc<NativeBootstrapProxy> {
    bootstrap_plugin().bootstrap_factories()[0]
        .create_provider(
            ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af").unwrap(),
            config,
            None,
            None,
            drasi_computation_plugin_sdk::BootstrapSession::default(),
        )
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn native_initial_snapshot_does_not_require_a_restart_refresh_signal() -> anyhow::Result<()> {
    let provider = provider(json!({"mode": "no-snapshot", "count": 2}));
    provider.prepare().await?;
    assert!(!provider.has_pending_snapshot()?);
    let mut snapshot = provider.snapshot().await?;
    let mut received = 0;
    while let Some(envelope) = snapshot.changes.next().await {
        assert_eq!(envelope?.changes().operations().len(), 1);
        received += 1;
    }
    assert_eq!(received, 2);
    assert!(
        provider.snapshot().await.is_err(),
        "preparation permits only one snapshot"
    );
    provider.stop().await?;
    Ok(())
}

#[derive(Default)]
struct State {
    value: Mutex<Option<Bytes>>,
    fail_write: bool,
    pending_write: bool,
    writes: AtomicUsize,
}
#[async_trait]
impl BootstrapState for State {
    fn durability(&self) -> StorageDurability {
        StorageDurability::LOCAL_POWER_LOSS
    }
    async fn read(&self) -> anyhow::Result<Option<Bytes>> {
        Ok(self.value.lock().unwrap().clone())
    }
    async fn write(&self, state: Bytes) -> anyhow::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.pending_write {
            std::future::pending::<()>().await;
        }
        anyhow::ensure!(!self.fail_write, "host refused initialization write");
        *self.value.lock().unwrap() = Some(state);
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_retirement_holds_reuse_and_terminal_cleanup_cannot_resume(
) -> anyhow::Result<()> {
    for resolution in 0..4 {
        let provider = provider(json!({}));
        assert!(provider.freeze_for_retirement().is_err());
        provider.prepare().await?;
        assert!(provider.freeze_for_retirement().is_err());
        provider.stop().await?;
        let lease = provider.freeze_for_retirement()?;
        assert!(provider.freeze_for_retirement().is_err());
        assert!(provider.prepare().await.is_err());
        assert!(provider.snapshot().await.is_err());
        provider.stop().await?;
        match resolution {
            0 => lease.resume(),
            1 => lease.retire(),
            2 => drop(lease),
            _ => {
                ResourceCleanup::shutdown(provider.as_ref()).await?;
                lease.resume();
            }
        }
        assert_eq!(provider.prepare().await.is_ok(), resolution == 0);
        ResourceCleanup::shutdown(provider.as_ref()).await?;
        assert!(provider.prepare().await.is_err());
        assert!(provider.freeze_for_retirement().is_err());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_c_boundary_preserves_state_stream_and_final_handover(
) -> anyhow::Result<()> {
    assert!(
        plugin().bootstrap_factories().is_empty(),
        "old native negotiation must not grant a new service"
    );
    let provider = provider(json!({}));
    assert!(provider.has_pending_snapshot().is_err());
    assert!(provider.completion_state().is_err());
    let state = State::default();
    assert_eq!(
        provider.prepare_with_state(&state).await?,
        BootstrapPreparation::Ready
    );
    assert!(provider.has_pending_snapshot()?);
    assert_eq!(state.writes.load(Ordering::SeqCst), 1);
    let mut snapshot = provider.snapshot_with_state(&state).await?;
    assert_eq!(
        snapshot.watermarks[0].position.as_deref(),
        Some([0, 255, 128].as_slice())
    );
    let mut count = 0;
    while let Some(envelope) = snapshot.changes.next().await {
        envelope?;
        count += 1;
    }
    assert_eq!(count, 3);
    let watermarks = provider.complete_snapshot().await?;
    assert_eq!(watermarks[0].sequence, u64::MAX);
    assert_eq!(
        watermarks[0].source_id.as_ref(),
        Some(&ComponentId::try_new("00000000-0000-0000-0000-00000000004d").expect("fixture UUID"))
    );
    assert_eq!(watermarks[0].position.as_deref(), Some([255, 0].as_slice()));
    assert_eq!(
        provider.completion_state()?.as_deref(),
        Some(b"complete".as_slice())
    );
    assert_eq!(
        state.read().await?.as_deref(),
        Some(b"initializing".as_slice()),
        "final state belongs in the query commit, not an eager write"
    );
    provider.stop().await?;
    assert!(provider.has_pending_snapshot().is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_failed_or_cancelled_state_cannot_be_ignored() -> anyhow::Result<()> {
    let provider = provider(json!({"mode":"ignore-error"}));
    let state = State {
        fail_write: true,
        ..Default::default()
    };
    let error = provider.prepare_with_state(&state).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed, cancelled or unfinished"),
        "{error:#}"
    );
    assert!(provider.prepare().await.is_err());
    provider.stop().await?;

    let state = State {
        pending_write: true,
        ..Default::default()
    };
    assert!(tokio::time::timeout(
        Duration::from_millis(20),
        provider.prepare_with_state(&state)
    )
    .await
    .is_err());
    assert_eq!(state.writes.load(Ordering::SeqCst), 1);
    assert!(provider.prepare().await.is_err());
    provider.stop().await?;
    assert_eq!(provider.prepare().await?, BootstrapPreparation::Ready);
    provider.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_drop_and_cancelled_cleanup_retain_the_worker_owner() -> anyhow::Result<()>
{
    let provider = provider(json!({"mode":"pending", "cleanupDelayMs":75}));
    provider.prepare().await?;
    let mut snapshot = provider.snapshot().await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), snapshot.changes.next())
            .await
            .is_err()
    );
    let error = provider.stop().await.unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<sdk::transport::Failure>()
            .unwrap()
            .code,
        abi::status::BUSY
    );
    drop(snapshot);
    assert!(provider.prepare().await.is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(5), provider.stop())
            .await
            .is_err()
    );
    assert!(provider.snapshot().await.is_err());
    provider.stop().await?;
    provider.prepare().await?;
    drop(provider.snapshot().await?);
    provider.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_stale_stream_cannot_read_or_cancel_its_replacement() -> anyhow::Result<()>
{
    let provider = provider(json!({}));
    provider.prepare().await?;
    let mut old = provider.snapshot().await?;
    provider.stop().await?;
    provider.prepare().await?;
    let mut replacement = provider.snapshot().await?;
    let failure = old.changes.next().await.unwrap().unwrap_err();
    assert_eq!(
        failure
            .downcast_ref::<sdk::transport::Failure>()
            .unwrap()
            .code,
        abi::status::CLOSED
    );
    drop(old);
    let mut count = 0;
    while let Some(value) = replacement.changes.next().await {
        value?;
        count += 1;
    }
    assert_eq!(count, 3);
    provider.complete_snapshot().await?;
    provider.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_bounds_errors_and_panics_require_cleanup() -> anyhow::Result<()> {
    for mode in ["oversize", "empty"] {
        let provider = provider(json!({"mode":mode}));
        let state = State::default();
        assert!(provider.prepare_with_state(&state).await.is_err());
        assert_eq!(state.writes.load(Ordering::SeqCst), 0);
        assert!(provider.prepare().await.is_err());
        provider.stop().await?;
    }
    for mode in ["stream-error", "panic"] {
        let provider = provider(json!({"mode":mode}));
        provider.prepare().await?;
        let mut snapshot = provider.snapshot().await?;
        assert!(snapshot.changes.next().await.unwrap().is_err());
        assert!(snapshot.changes.next().await.is_none());
        assert!(provider.complete_snapshot().await.is_err());
        provider.stop().await?;
    }
    let provider = provider(json!({"mode":"bad-completion", "count":0}));
    provider.prepare().await?;
    assert!(provider.snapshot().await?.changes.next().await.is_none());
    assert!(provider.complete_snapshot().await.is_err());
    assert!(provider.completion_state().is_err());
    provider.stop().await?;
    Ok(())
}

fn reprepare_factories() -> anyhow::Result<Vec<Arc<NativeBootstrapFactory>>> {
    let linked = NativePlugin::from_definition(sdk::PluginDefinition::new_with_bootstrap(
        "fixture/bootstrap-reprepare",
        "1",
        vec![],
        vec![Arc::new(bootstrap_fixture::Factory { progress: false })],
        vec![GraphChangeCodec::schema()],
    )?)?;
    let dynamic = super::super::factory::recovery_tests::sqlite_plugin()?;
    Ok(vec![
        linked.bootstrap_factories()[0].clone(),
        dynamic.bootstrap_factories()[0].clone(),
    ])
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_non_ready_reprepares_without_stop_static_and_dynamic(
) -> anyhow::Result<()> {
    for factory in reprepare_factories()? {
        for (mode, initial) in [
            ("refresh-once", BootstrapPreparation::RefreshVolatile),
            ("reset-once", BootstrapPreparation::ResetRequired),
        ] {
            for stateful in [false, true] {
                let provider = factory.create_provider(
                    ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
                    json!({"mode":mode,"count":2}),
                    None,
                    None,
                    drasi_computation_plugin_sdk::BootstrapSession::default(),
                )?;
                let before_reset = State::default();
                let after_reset = State::default();
                let preparation = if stateful {
                    provider.prepare_with_state(&before_reset).await?
                } else {
                    provider.prepare().await?
                };
                assert_eq!(preparation, initial);
                assert!(provider.has_pending_snapshot()?);
                let preparation = if stateful {
                    provider.prepare_with_state(&after_reset).await?
                } else {
                    provider.prepare().await?
                };
                assert_eq!(preparation, BootstrapPreparation::Ready);
                let mut snapshot = if stateful {
                    assert_eq!(before_reset.writes.load(Ordering::SeqCst), 1);
                    assert_eq!(after_reset.writes.load(Ordering::SeqCst), 1);
                    provider.snapshot_with_state(&after_reset).await?
                } else {
                    provider.snapshot().await?
                };
                let mut count = 0;
                while let Some(value) = snapshot.changes.next().await {
                    assert_eq!(value?.changes().operations().len(), 1);
                    count += 1;
                }
                assert_eq!(count, 2);
                provider.complete_snapshot().await?;
                provider.stop().await?;
                assert_eq!(provider.prepare().await?, BootstrapPreparation::Ready);
                assert!(
                    provider.prepare().await.is_err(),
                    "duplicate Ready preparation must fail"
                );
                assert!(
                    provider.snapshot().await.is_err(),
                    "invalid preparation still requires cleanup"
                );
                provider.stop().await?;
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_non_ready_retry_preserves_snapshot_and_cancellation_guards(
) -> anyhow::Result<()> {
    for factory in reprepare_factories()? {
        for (mode, initial) in [
            ("refresh", BootstrapPreparation::RefreshVolatile),
            ("reset", BootstrapPreparation::ResetRequired),
        ] {
            let provider = factory.create_provider(
                ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
                json!({"mode":mode}),
                None,
                None,
                drasi_computation_plugin_sdk::BootstrapSession::default(),
            )?;
            assert_eq!(provider.prepare().await?, initial);
            assert_eq!(provider.prepare().await?, initial);
            let snapshot = provider.snapshot().await?;
            assert!(
                provider.prepare().await.is_err(),
                "a started snapshot cannot be re-prepared"
            );
            assert!(
                provider.snapshot().await.is_err(),
                "a snapshot remains single-use"
            );
            drop(snapshot);
            provider.stop().await?;
        }
        let provider = factory.create_provider(
            ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
            json!({"mode":"refresh-once"}),
            None,
            None,
            drasi_computation_plugin_sdk::BootstrapSession::default(),
        )?;
        assert_eq!(
            provider.prepare().await?,
            BootstrapPreparation::RefreshVolatile
        );
        let pending = State {
            pending_write: true,
            ..Default::default()
        };
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            provider.prepare_with_state(&pending)
        )
        .await
        .is_err());
        assert_eq!(pending.writes.load(Ordering::SeqCst), 1);
        assert!(
            provider.prepare().await.is_err(),
            "cancelled retry must not become reusable"
        );
        assert!(provider.snapshot().await.is_err());
        provider.stop().await?;
        assert_eq!(provider.prepare().await?, BootstrapPreparation::Ready);
        provider.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_non_ready_query_reset_reprepares_static_and_dynamic() -> anyhow::Result<()>
{
    for factory in reprepare_factories()? {
        for mode in ["refresh-once", "reset-once"] {
            let owner = progress();
            let bootstrap = factory.create_provider(
                ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
                json!({"mode":mode, "count":2, "durability":StorageDurability::VOLATILE}),
                None,
                None,
                drasi_computation_plugin_sdk::BootstrapSession::default(),
            )?;
            let mut query = ContinuousQueryTransformer::new_with_options(
                definition(),
                Arc::new(drasi_core::computation::InMemoryComputationProvider),
                QueryOptions {
                    recovery: QueryRecoveryPolicy::AutoReset,
                    ..Default::default()
                },
            )
            .await?
            .with_bootstrap(bootstrap)
            .with_source_progress(owner.clone())?;
            query
                .start(&crate::computation::test_hooks::start_ctx())
                .await?;
            assert_eq!(query.results().snapshot()?.rows.len(), 2);
            assert!(owner.snapshot().bootstrap_complete);
            assert!(!owner.snapshot().persistent);
            query
                .stop(&crate::computation::test_hooks::stop_ctx())
                .await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_preparation_choices_and_protocol_order_are_explicit() -> anyhow::Result<()>
{
    for (mode, expected) in [
        ("reset", BootstrapPreparation::ResetRequired),
        ("refresh", BootstrapPreparation::RefreshVolatile),
        ("no-snapshot", BootstrapPreparation::Ready),
    ] {
        let provider = provider(json!({"mode":mode}));
        assert_eq!(provider.prepare().await?, expected);
        assert_eq!(provider.has_pending_snapshot()?, mode != "no-snapshot");
        provider.stop().await?;
    }
    let provider = provider(json!({}));
    assert!(provider.snapshot().await.is_err());
    provider.stop().await?;
    provider.prepare().await?;
    assert!(provider.complete_snapshot().await.is_err());
    provider.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_progress_requires_actual_owner_and_stable_reader() -> anyhow::Result<()> {
    let plugin = bootstrap_plugin();
    let factory = &plugin.bootstrap_factories()[1];
    assert!(factory
        .create_provider(
            ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
            json!({}),
            Some(scope()),
            None,
            drasi_computation_plugin_sdk::BootstrapSession::default()
        )
        .is_err());
    let owner = progress();
    assert!(factory
        .create_provider(
            ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
            json!({"mode":"forge-reader"}),
            Some(scope()),
            Some(owner.clone()),
            drasi_computation_plugin_sdk::BootstrapSession::default(),
        )
        .is_err());
    let changed = factory.create_provider(
        ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
        json!({"mode":"change-reader"}),
        Some(scope()),
        Some(owner.clone()),
        drasi_computation_plugin_sdk::BootstrapSession::default(),
    )?;
    assert!(changed
        .prepare()
        .await
        .unwrap_err()
        .to_string()
        .contains("changed its recovery reader"));
    changed.stop().await?;

    let bootstrap = factory.create_provider(
        ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
        json!({}),
        Some(scope()),
        Some(owner.clone()),
        drasi_computation_plugin_sdk::BootstrapSession::default(),
    )?;
    assert!(Arc::ptr_eq(bootstrap.source_progress().unwrap(), &owner));
    let mut query = ContinuousQueryTransformer::new(
        definition(),
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await?
    .with_bootstrap(bootstrap)
    .with_source_progress(progress())?;
    let error = query
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("actual progress owner"),
        "{error:#}"
    );
    query
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    Ok(())
}

fn definition() -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "native-bootstrap".into(),
        id: ComponentId::try_new("6ddf3eb2-187f-5740-a6ba-a07f98257ca8").unwrap(),
        query: "MATCH (n:Item) RETURN n.value AS value".into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("results").unwrap(),
        outbox_capacity: NonZeroUsize::new(8).unwrap(),
    }
}
async fn persisted_query(
    path: &std::path::Path,
    owner: Arc<QuerySourceProgress>,
    bootstrap: Arc<dyn ComputationBootstrapProvider>,
) -> anyhow::Result<ContinuousQueryTransformer> {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    ContinuousQueryTransformer::new_with_options(
        definition(),
        Arc::new(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        )),
        QueryOptions {
            recovery: QueryRecoveryPolicy::AutoReset,
            ..Default::default()
        },
    )
    .await?
    .with_bootstrap(bootstrap)
    .with_source_progress(owner)
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_real_library_commits_snapshot_and_recovers_without_resnapshot(
) -> anyhow::Result<()> {
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    let factory = &plugin.bootstrap_factories()[1];
    let directory = tempfile::tempdir()?;
    for count in [3, 99] {
        let owner = progress();
        let bootstrap = factory.create_provider(
            ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
            json!({"count":count, "durability":StorageDurability::LOCAL_PROCESS_RESTART}),
            Some(scope()),
            Some(owner.clone()),
            drasi_computation_plugin_sdk::BootstrapSession::default(),
        )?;
        let mut query = persisted_query(directory.path(), owner.clone(), bootstrap).await?;
        query
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
        let rows = query.results().snapshot()?.rows;
        assert_eq!(
            rows.len(),
            3,
            "reconstruction must reuse the committed snapshot"
        );
        assert!(owner.snapshot().bootstrap_complete);
        assert_eq!(
            owner.snapshot().checkpoints
                [&SourceProgressKey::Stream(StreamId::try_new("fixture/out")?)]
                .sequence,
            3
        );
        assert_eq!(
            owner.snapshot().checkpoints[&SourceProgressKey::Source(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID")
            )]
                .sequence,
            u64::MAX
        );
        query
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_real_library_preserves_initialization_intent_after_cancelled_start(
) -> anyhow::Result<()> {
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    let directory = tempfile::tempdir()?;
    for mode in ["pending", "require-replay"] {
        let owner = progress();
        let bootstrap = plugin.bootstrap_factories()[1].create_provider(
            ComponentId::try_new("1d4f4a3d-6cce-5052-b017-09f1341b67af")?,
            json!({"mode":mode, "durability":StorageDurability::LOCAL_PROCESS_RESTART}),
            Some(scope()),
            Some(owner.clone()),
            drasi_computation_plugin_sdk::BootstrapSession::default(),
        )?;
        let mut query = persisted_query(directory.path(), owner.clone(), bootstrap.clone()).await?;
        if mode == "pending" {
            {
                let start_context = crate::computation::test_hooks::start_ctx();
                let start = query.start(&start_context);
                tokio::pin!(start);
                tokio::select! {
                    result = &mut start => panic!("pending snapshot unexpectedly completed: {result:?}"),
                    prepared = tokio::time::timeout(Duration::from_secs(10), async {
                        while bootstrap.has_pending_snapshot().is_err() { tokio::task::yield_now().await; }
                    }) => { prepared?; }
                }
            }
            assert!(!owner.snapshot().bootstrap_complete);
        } else {
            query
                .start(&crate::computation::test_hooks::start_ctx())
                .await?;
            assert_eq!(query.results().snapshot()?.rows.len(), 3);
        }
        query
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
    }
    Ok(())
}

#[test]
fn native_bootstrap_malformed_negotiation_rejects_before_entry() {
    static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    unsafe extern "C" fn entry(_: *mut abi::PluginHandle) -> abi::Status {
        ENTERED.store(true, Ordering::Release);
        abi::Status::ok()
    }
    unsafe extern "C" fn extension() -> *const abi::bootstrap::PluginBootstrapV1 {
        static TABLE: abi::bootstrap::PluginBootstrapV1 = abi::bootstrap::PluginBootstrapV1 {
            header: abi::Header::new::<abi::bootstrap::PluginBootstrapV1>(),
            version: abi::bootstrap::VERSION + 1,
            reserved: 0,
            factories: None,
            describe: None,
            create: None,
        };
        &TABLE
    }
    assert!(unsafe {
        NativePlugin::from_entry_points_with_bootstrap(
            drasi_computation_plugin_metadata,
            entry,
            None,
            None,
            Some(extension),
        )
    }
    .is_err());
    assert!(!ENTERED.load(Ordering::Acquire));
}
