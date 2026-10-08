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
        QuerySourceProgress::new("native-bootstrap", ComponentId::try_new("query").unwrap())
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
            ComponentId::try_new("bootstrap").unwrap(),
            config,
            None,
            None,
        )
        .unwrap()
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
    assert_eq!(watermarks[0].source_id.as_deref(), Some("source"));
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
            ComponentId::try_new("bootstrap")?,
            json!({}),
            Some(scope()),
            None
        )
        .is_err());
    let owner = progress();
    assert!(factory
        .create_provider(
            ComponentId::try_new("bootstrap")?,
            json!({"mode":"forge-reader"}),
            Some(scope()),
            Some(owner.clone()),
        )
        .is_err());
    let changed = factory.create_provider(
        ComponentId::try_new("bootstrap")?,
        json!({"mode":"change-reader"}),
        Some(scope()),
        Some(owner.clone()),
    )?;
    assert!(changed
        .prepare()
        .await
        .unwrap_err()
        .to_string()
        .contains("changed its recovery reader"));
    changed.stop().await?;

    let bootstrap = factory.create_provider(
        ComponentId::try_new("bootstrap")?,
        json!({}),
        Some(scope()),
        Some(owner.clone()),
    )?;
    assert!(Arc::ptr_eq(bootstrap.source_progress().unwrap(), &owner));
    let mut query = ContinuousQueryTransformer::new(
        definition(),
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await?
    .with_bootstrap(bootstrap)
    .with_source_progress(progress())?;
    let error = query.start().await.unwrap_err();
    assert!(
        error.to_string().contains("actual progress owner"),
        "{error:#}"
    );
    query.stop().await?;
    Ok(())
}

fn definition() -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "native-bootstrap".into(),
        id: ComponentId::try_new("query").unwrap(),
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
            ComponentId::try_new("bootstrap")?,
            json!({"count":count, "durability":StorageDurability::LOCAL_PROCESS_RESTART}),
            Some(scope()),
            Some(owner.clone()),
        )?;
        let mut query = persisted_query(directory.path(), owner.clone(), bootstrap).await?;
        query.start().await?;
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
            owner.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].sequence,
            u64::MAX
        );
        query.stop().await?;
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
            ComponentId::try_new("bootstrap")?,
            json!({"mode":mode, "durability":StorageDurability::LOCAL_PROCESS_RESTART}),
            Some(scope()),
            Some(owner.clone()),
        )?;
        let mut query = persisted_query(directory.path(), owner.clone(), bootstrap.clone()).await?;
        if mode == "pending" {
            {
                let start = query.start();
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
            query.start().await?;
            assert_eq!(query.results().snapshot()?.rows.len(), 3);
        }
        query.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_recipe_uses_declared_owners_and_unresolved_secrets() -> anyhow::Result<()>
{
    use crate::management::{HostManagementResources, NativeBootstrapConfig};
    use drasi_lib::management::ManagementResourceResolver;
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    let mut registry = crate::PluginRegistry::new();
    registry.register_computation_plugin(plugin.clone())?;
    let resolver = HostManagementResources::new(
        &registry,
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        None,
    )?;
    let specification = ResourceSpecification {
        id: ResourceId::try_new("bootstrap")?,
        role: ResourceRole::Bootstrap,
        ownership: ResourceOwnership::Graph,
        binding: "bootstrap".into(),
    };
    let progress_id = ResourceId::try_new("progress")?;
    let secrets_id = ResourceId::try_new("secrets")?;
    let config = NativeBootstrapConfig {
        component: ComponentId::try_new("query")?,
        implementation: plugin.bootstrap_factories()[1]
            .metadata()
            .implementation
            .clone(),
        configuration_version: 1,
        configuration: BTreeMap::from([
            (
                "durability".into(),
                ConfigurationValue::Literal(json!(StorageDurability::LOCAL_PROCESS_RESTART)),
            ),
            (
                "testSecret".into(),
                ConfigurationValue::Reference {
                    resource: secrets_id.clone(),
                    key: "secret:BOOTSTRAP".into(),
                    secret: true,
                },
            ),
        ]),
        source_progress: Some(progress_id.clone()),
    };
    let mut recipe = serde_json::to_value(&config)?;
    recipe["kind"] = json!("nativeBootstrap");
    assert!(resolver
        .resolve(
            "native-bootstrap",
            "native-bootstrap",
            &specification,
            &recipe
        )
        .await
        .is_err());
    let owner = progress();
    let secret_spec = ResourceSpecification {
        id: secrets_id.clone(),
        role: ResourceRole::SecretStore,
        ownership: ResourceOwnership::Graph,
        binding: "secrets".into(),
    };
    let dependencies = BTreeMap::from([
        (
            progress_id,
            ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(owner.clone())),
            ),
        ),
        (
            secrets_id,
            resolver
                .resolve(
                    "native-bootstrap",
                    "native-bootstrap",
                    &secret_spec,
                    &json!({"kind":"configuration","secrets":{"BOOTSTRAP":"fixture-only"}}),
                )
                .await?,
        ),
    ]);
    let roles = dependencies
        .iter()
        .map(|(id, handle)| (id.clone(), handle.role()))
        .collect();
    let mut invalid = config.clone();
    invalid.configuration.insert(
        "testSecret".into(),
        ConfigurationValue::Literal(json!("fixture-only")),
    );
    assert!(invalid
        .validate_configuration(
            &registry.computation_bootstrap_factories()?,
            &roles,
            &dependencies
        )
        .is_err());
    let handle = resolver
        .resolve_with_dependencies(
            "native-bootstrap",
            "native-bootstrap",
            &specification,
            &recipe,
            &dependencies,
        )
        .await?;
    assert!(handle.has_cleanup());
    let provider = handle.get::<QueryBootstrapResource>()?.0.clone();
    provider.validate_query_scope("native-bootstrap", "native-bootstrap", &config.component)?;
    for (instance, graph, component) in [
        ("other", "native-bootstrap", "query"),
        ("native-bootstrap", "other", "query"),
        ("native-bootstrap", "native-bootstrap", "other"),
    ] {
        assert!(provider
            .validate_query_scope(instance, graph, &ComponentId::try_new(component)?)
            .is_err());
    }
    let directory = tempfile::tempdir()?;
    let mut query = persisted_query(directory.path(), owner, provider.clone()).await?;
    query.start().await?;
    assert_eq!(query.results().snapshot()?.rows.len(), 3);
    query.stop().await?;
    let id = specification.id.clone();
    let service = super::plugin()
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == "test/service")
        .expect("service fixture")
        .clone();
    let mut graph = ComputationGraph::builder("native-bootstrap")
        .declare_resource(specification)?
        .provide_resource(id, handle)?
        .component(
            service.specification(ComponentId::try_new("owner")?, json!({}))?,
            service,
        )
        .build()?;
    graph.start()?.await?;
    graph.dispose().await?;
    assert!(
        provider.prepare().await.is_err(),
        "retired resources cannot be revived"
    );
    assert!(provider
        .validate_query_scope("native-bootstrap", "native-bootstrap", &config.component)
        .is_err());
    assert!(!serde_json::to_string(&recipe)?.contains("fixture-only"));
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
