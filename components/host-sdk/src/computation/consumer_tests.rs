// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationTransaction},
    interface::FailureMode,
    models::ElementValue,
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use std::path::Path;

const GRAPH: &str = "native-consumer";

#[path = "consumer_handoff_tests.rs"]
mod handoff_tests;

struct BatchSource {
    descriptor: ComponentDescriptor,
    input: Option<ChangeEnvelope>,
}
#[async_trait]
impl ComputationComponent for BatchSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for BatchSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.input.take().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").unwrap(),
            envelope,
        }))
    }
}
pub(in crate::computation) fn plugin() -> anyhow::Result<Arc<NativePlugin>> {
    unsafe {
        NativePlugin::from_entry_points_with_consumer(
            drasi_computation_plugin_metadata,
            drasi_computation_plugin_entry,
            None,
            None,
            None,
            Some(drasi_computation_plugin_consumer_v1),
        )
    }
}
pub(in crate::computation) fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).unwrap()
}
pub(in crate::computation) fn scope() -> sdk::Scope {
    sdk::Scope {
        instance_id: GRAPH.into(),
        graph_id: GRAPH.into(),
        generation: 1,
    }
}
pub(in crate::computation) fn options() -> DeliveryOptions {
    DeliveryOptions {
        scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
        max_streams: NonZeroUsize::new(2).unwrap(),
        receipts_per_stream: NonZeroUsize::new(2).unwrap(),
        retry: DeliveryRetryPolicy {
            max_attempts: NonZeroUsize::new(2).unwrap(),
            delay: Duration::ZERO,
        },
    }
}
pub(in crate::computation) fn provider(
    path: &Path,
) -> anyhow::Result<Arc<dyn ComputationIndexProvider>> {
    Ok(Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    )))
}
pub(in crate::computation) fn factory(
    plugin: &NativePlugin,
    mode: sdk::ConsumerMode,
) -> Arc<NativeFactory> {
    plugin
        .factories()
        .iter()
        .find(|factory| factory.consumer_mode() == Some(mode))
        .unwrap()
        .clone()
}
pub(in crate::computation) fn batch() -> anyhow::Result<InputEnvelope> {
    let producer: GraphProducerIdentity = serde_json::from_value(json!({
        "construction_scope": GRAPH, "graph_id": GRAPH, "component_id": "source",
        "stream": "out", "incarnation": "c218b0c6-8803-48df-aa4c-d5350f0e6453",
        "persistent": true
    }))?;
    let changes = (0..3)
        .map(|index| SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &index.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::from(json!({"value": index})),
            },
        })
        .collect::<Vec<_>>();
    let mut envelope = GraphChangeCodec::derive_changes(
        &super::envelope(&id("source"), 1),
        &changes,
        producer.stream().clone(),
        1,
    )?;
    GraphProducerProgress::annotate(&mut envelope, &producer, 1)?;
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope,
    })
}
fn effects(path: &Path) -> anyhow::Result<(usize, usize, usize)> {
    Ok(rusqlite::Connection::open(path)?.query_row(
        "SELECT (SELECT count(*) FROM effects), count(*), count(DISTINCT id) FROM attempts",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}
async fn count(provider: &dyn ComputationIndexProvider) -> anyhow::Result<Option<ElementValue>> {
    let transaction =
        ComputationTransaction::try_new(provider.create_indexes(GRAPH, "consumer").await?)?;
    let prefix: String = "consumer"
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let reference = ElementReference::new(&format!("transaction-step/{prefix}/values"), "count");
    let element = transaction
        .run(async {
            Ok(transaction
                .resources()
                .indexes()
                .element_index
                .get_element(&reference)
                .await?)
        })
        .await?;
    let value = match element.as_deref() {
        Some(Element::Node { properties, .. }) => properties.get("value").cloned(),
        None => None,
        _ => anyhow::bail!("invalid fixture state"),
    };
    transaction.shutdown().await?;
    Ok(value)
}
async fn external_reconstruction(plugin: &NativePlugin) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let destination = directory.path().join("effects.db");
    let provider = provider(&directory.path().join("progress"))?;
    let factory = factory(plugin, sdk::ConsumerMode::External);
    assert!(factory.create_component(id("consumer"), json!({})).is_err());
    for mode in ["fail-second", "retry-once", "normal"] {
        let mut consumer = factory
            .create_consumer(
                id("consumer"),
                json!({"path":destination, "mode":mode}),
                scope(),
                provider.clone(),
                options(),
            )
            .await?;
        consumer.start().await?;
        let result = consumer.handle(batch()?).await;
        let progress = consumer.progress().await?;
        if mode == "fail-second" {
            assert!(result.is_err());
            assert_eq!(progress[0].completed_operations, 1);
            assert_eq!(progress[0].handled_sequence, 0);
            assert!(consumer.handle(batch()?).await.is_err());
        } else {
            result?;
            assert_eq!(progress[0].completed_operations, 3);
            assert_eq!(progress[0].handled_sequence, 1);
        }
        consumer.stop().await?;
    }
    // Second operation repeated after an uncertain destination response, with the same key.
    assert_eq!(effects(&destination)?, (3, 4, 3));
    Ok(())
}
async fn transactional_reconstruction(plugin: &NativePlugin) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = provider(directory.path())?;
    let factory = factory(plugin, sdk::ConsumerMode::Transactional);
    for (mode, expected) in [("fail-second", 1), ("normal", 3), ("normal", 3)] {
        let mut consumer = factory
            .create_consumer(
                id("consumer"),
                json!({"mode":mode}),
                scope(),
                provider.clone(),
                options(),
            )
            .await?;
        consumer.start().await?;
        let result = consumer.handle(batch()?).await;
        if mode == "fail-second" {
            assert!(result.is_err());
        } else {
            result?;
        }
        assert_eq!(consumer.progress().await?[0].completed_operations, expected);
        consumer.stop().await?;
        assert_eq!(
            count(provider.as_ref()).await?,
            Some(ElementValue::Integer(expected as i64))
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_external_retry_and_partial_reconstruction() -> anyhow::Result<()> {
    external_reconstruction(plugin()?.as_ref()).await
}
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_transactional_state_and_completion_commit_together() -> anyhow::Result<()>
{
    transactional_reconstruction(plugin()?.as_ref()).await
}
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_separate_library_preserves_both_completion_modes() -> anyhow::Result<()> {
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    external_reconstruction(&plugin).await?;
    transactional_reconstruction(&plugin).await
}
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_ignored_state_errors_cannot_commit() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = provider(directory.path())?;
    let mut consumer = factory(plugin()?.as_ref(), sdk::ConsumerMode::Transactional)
        .create_consumer(
            id("consumer"),
            json!({"mode":"ignore-state-error"}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    consumer.start().await?;
    assert!(consumer.handle(batch()?).await.is_err());
    assert_eq!(consumer.progress().await?[0].completed_operations, 0);
    consumer.stop().await?;
    assert_eq!(count(provider.as_ref()).await?, None);
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_cancelled_transaction_requires_cleanup_and_replays() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let signal = directory.path().join("entered");
    let provider = provider(&directory.path().join("progress"))?;
    let factory = factory(plugin()?.as_ref(), sdk::ConsumerMode::Transactional);
    let mut consumer = factory
        .create_consumer(
            id("consumer"),
            json!({"mode":"pending", "signal":signal}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    consumer.start().await?;
    {
        let handle = consumer.handle(batch()?);
        tokio::pin!(handle);
        tokio::select! {
            result = &mut handle => panic!("pending handler completed: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(10), async {
                while !signal.exists() { tokio::task::yield_now().await; }
            }) => { entered?; }
        }
    }
    assert!(consumer.start().await.is_err());
    consumer.stop().await?;
    assert_eq!(count(provider.as_ref()).await?, None);
    let mut replacement = factory
        .create_consumer(
            id("consumer"),
            json!({}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    replacement.start().await?;
    replacement.handle(batch()?).await?;
    replacement.stop().await?;
    assert_eq!(
        count(provider.as_ref()).await?,
        Some(ElementValue::Integer(3))
    );
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_cannot_reinterpret_existing_handling_mode() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = provider(&directory.path().join("progress"))?;
    let plugin = plugin()?;
    let mut consumer = factory(&plugin, sdk::ConsumerMode::External)
        .create_consumer(
            id("consumer"),
            json!({"path":directory.path().join("effects.db")}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    consumer.start().await?;
    consumer.handle(batch()?).await?;
    consumer.stop().await?;
    let mut consumer = factory(&plugin, sdk::ConsumerMode::Transactional)
        .create_consumer(id("consumer"), json!({}), scope(), provider, options())
        .await?;
    assert!(consumer.start().await.is_err());
    consumer.stop().await?;
    Ok(())
}
#[test]
fn native_consumer_malformed_negotiation_rejects_before_entry() {
    static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    unsafe extern "C" fn entry(_: *mut abi::PluginHandle) -> abi::Status {
        ENTERED.store(true, Ordering::Release);
        abi::Status::ok()
    }
    unsafe extern "C" fn extension() -> *const abi::consumer::PluginConsumerV1 {
        static TABLE: abi::consumer::PluginConsumerV1 = abi::consumer::PluginConsumerV1 {
            header: abi::Header::new::<abi::consumer::PluginConsumerV1>(),
            version: abi::consumer::VERSION + 1,
            reserved: 0,
            factory: None,
            create: None,
            inspect: None,
            begin: None,
            end_batch: None,
        };
        &TABLE
    }
    assert!(unsafe {
        NativePlugin::from_entry_points_with_consumer(
            drasi_computation_plugin_metadata,
            entry,
            None,
            None,
            None,
            Some(extension),
        )
    }
    .is_err());
    assert!(!ENTERED.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_graph_factory_requires_and_resolves_delivery_resource(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let factory = factory(plugin()?.as_ref(), sdk::ConsumerMode::External);
    let mut specification = factory.specification(
        id("consumer"),
        json!({"path":directory.path().join("effects.db")}),
    )?;
    assert!(factory.validate(&specification).is_err());
    let resource = ResourceId::try_new("delivery")?;
    specification
        .dependencies
        .insert("consumer".into(), vec![resource.clone()]);
    factory.validate(&specification)?;
    assert!(super::plugin().factories()[6]
        .validate(&specification)
        .is_err());
    let mut graph = ComputationGraph::builder(GRAPH)
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Borrowed,
            binding: "delivery".into(),
        })?
        .provide_resource(
            resource,
            ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(NativeConsumerResource {
                    provider: provider(&directory.path().join("progress"))?,
                    options: options(),
                }),
            ),
        )?
        .component(specification, factory)
        .source(Box::new(BatchSource {
            descriptor: ComponentDescriptor::try_new(
                id("source"),
                vec![super::port(PortDirection::Output)],
            )?,
            input: Some(batch()?.envelope),
        }))
        .bind_stream(
            Endpoint::new(id("source"), PortId::try_new("out")?),
            StreamId::try_new("out")?,
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(id("source"), PortId::try_new("out")?),
                Endpoint::new(id("consumer"), PortId::try_new("in")?),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()?;
    graph.start()?.await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if effects(&directory.path().join("effects.db"))? == (3, 3, 3) {
                break anyhow::Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    graph.shutdown().await?;
    assert_eq!(effects(&directory.path().join("effects.db"))?, (3, 3, 3));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_panic_requires_reconstruction_after_cleanup() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let factory = factory(plugin()?.as_ref(), sdk::ConsumerMode::Transactional);
    let provider = provider(directory.path())?;
    let mut consumer = factory
        .create_consumer(
            id("consumer"),
            json!({"mode":"panic"}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    consumer.start().await?;
    assert!(consumer.handle(batch()?).await.is_err());
    consumer.stop().await?;
    assert!(consumer.start().await.is_err());
    consumer.stop().await?;
    assert_eq!(count(provider.as_ref()).await?, None);
    let mut replacement = factory
        .create_consumer(id("consumer"), json!({}), scope(), provider, options())
        .await?;
    replacement.start().await?;
    replacement.handle(batch()?).await?;
    replacement.stop().await?;
    Ok(())
}

#[test]
fn native_consumer_factory_contract_rejects_acceptance_snapshot_and_independent_control() {
    let mut metadata = sdk::Factory::metadata(&super::consumer_fixture::Factory(
        sdk::ConsumerMode::External,
    ));
    sdk::ConsumerMode::External
        .validate_factory(&metadata)
        .unwrap();
    sdk::ConsumerMode::Transactional
        .validate_factory(&metadata)
        .unwrap();
    metadata.capabilities.control = true;
    assert!(sdk::ConsumerMode::Transactional
        .validate_factory(&metadata)
        .is_err());
    assert!(sdk::ConsumerMode::External
        .validate_factory(&metadata)
        .is_ok());
    metadata.capabilities.control = false;
    metadata.capabilities.readiness = true;
    assert!(sdk::ConsumerMode::Transactional
        .validate_factory(&metadata)
        .is_err());
    metadata.capabilities.readiness = false;
    metadata.capabilities.snapshot = true;
    assert!(sdk::ConsumerMode::External
        .validate_factory(&metadata)
        .is_err());
    metadata.capabilities.snapshot = false;
    metadata.completion = Some(SinkCompletion::Accepted);
    assert!(sdk::ConsumerMode::External
        .validate_factory(&metadata)
        .is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_recipe_preserves_progress_and_isolates_instance_owners(
) -> anyhow::Result<()> {
    use crate::management::HostManagementResources;
    use drasi_lib::management::ManagementResourceResolver;
    let directory = tempfile::tempdir()?;
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    let resolver = HostManagementResources::new(
        &crate::PluginRegistry::new(),
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        None,
    )?
    .with_index_provider("disk", provider(directory.path())?)?;
    let indexes = ResourceSpecification {
        id: ResourceId::try_new("indexes")?,
        role: ResourceRole::IndexBackend,
        ownership: ResourceOwnership::Graph,
        binding: "indexes".into(),
    };
    let consumer = ResourceSpecification {
        id: ResourceId::try_new("delivery")?,
        role: ResourceRole::IndexBackend,
        ownership: ResourceOwnership::Graph,
        binding: "delivery".into(),
    };
    let recipe = json!({"kind":"nativeConsumer","failureScope":FailureMode::ProcessRestart,
        "maxStreams":2,"receiptsPerStream":2});
    assert!(resolver
        .resolve(GRAPH, GRAPH, &consumer, &recipe)
        .await
        .is_err());
    for (instance, existing) in [(GRAPH, false), ("another-instance", false), (GRAPH, true)] {
        let dependency = resolver
            .resolve(
                instance,
                GRAPH,
                &indexes,
                &json!({"kind":"indexes","provider":"disk"}),
            )
            .await?;
        let binding = resolver
            .resolve_with_dependencies(
                instance,
                GRAPH,
                &consumer,
                &recipe,
                &BTreeMap::from([(indexes.id.clone(), dependency)]),
            )
            .await?;
        let resource = binding.get::<NativeConsumerResource>()?;
        assert_eq!(
            count(resource.provider.as_ref()).await?,
            existing.then_some(ElementValue::Integer(3))
        );
        let mut scope = scope();
        scope.instance_id = instance.into();
        let mut sink = factory(&plugin, sdk::ConsumerMode::Transactional)
            .create_consumer(
                id("consumer"),
                json!({}),
                scope,
                resource.provider.clone(),
                resource.options.clone(),
            )
            .await?;
        sink.start().await?;
        sink.handle(batch()?).await?;
        sink.stop().await?;
        assert_eq!(
            count(resource.provider.as_ref()).await?,
            Some(ElementValue::Integer(3))
        );
    }
    Ok(())
}

struct ObservedProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    closed: Arc<AtomicUsize>,
    weakened: std::sync::atomic::AtomicBool,
    hold: Arc<std::sync::atomic::AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
}
struct ObservedCleanup {
    inner: Arc<dyn drasi_core::computation::ComputationResourceCleanup>,
    closed: Arc<AtomicUsize>,
    hold: Arc<std::sync::atomic::AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl drasi_core::computation::ComputationResourceCleanup for ObservedCleanup {
    fn cancel(&self) {
        self.inner.cancel();
    }
    async fn shutdown(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.entered.notify_one();
        if self.hold.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
        self.inner.shutdown().await?;
        self.closed.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}
#[async_trait]
impl ComputationIndexProvider for ObservedProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<
        drasi_core::computation::ComputationIndexes,
        drasi_core::interface::IndexError,
    > {
        let indexes = self.inner.create_indexes(graph, component).await?;
        let cleanup = Arc::new(ObservedCleanup {
            inner: indexes.cleanup().unwrap().clone(),
            closed: self.closed.clone(),
            hold: self.hold.clone(),
            entered: self.entered.clone(),
        });
        let indexes = indexes.with_cleanup(cleanup);
        Ok(if self.weakened.load(Ordering::Acquire) {
            indexes.with_durability(drasi_core::interface::StorageDurability::VOLATILE)
        } else {
            indexes
        })
    }
    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
}
fn observed(path: &Path) -> anyhow::Result<Arc<ObservedProvider>> {
    Ok(Arc::new(ObservedProvider {
        inner: provider(path)?,
        closed: Arc::new(AtomicUsize::new(0)),
        weakened: std::sync::atomic::AtomicBool::new(false),
        hold: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        entered: Arc::new(tokio::sync::Notify::new()),
    }))
}
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_rejected_construction_awaits_acquired_storage_cleanup(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = observed(directory.path())?;
    let factory = factory(plugin()?.as_ref(), sdk::ConsumerMode::Transactional);
    let mut unsupported = options();
    unsupported.scope = RecoveryScope::Failure(FailureMode::PowerLoss);
    assert!(factory
        .create_consumer(
            id("consumer"),
            json!({}),
            scope(),
            provider.clone(),
            unsupported,
        )
        .await
        .is_err());
    assert_eq!(provider.closed.load(Ordering::Acquire), 1);
    let mut consumer = factory
        .create_consumer(
            id("consumer"),
            json!({}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    consumer.start().await?;
    consumer.stop().await?;
    assert_eq!(provider.closed.load(Ordering::Acquire), 2);
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_cancelled_stop_retains_storage_until_cleanup_completes(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = observed(directory.path())?;
    let mut consumer = factory(plugin()?.as_ref(), sdk::ConsumerMode::Transactional)
        .create_consumer(
            id("consumer"),
            json!({}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    consumer.start().await?;
    provider.hold.store(true, Ordering::Release);
    {
        let stop = consumer.stop();
        tokio::pin!(stop);
        tokio::select! {
            result = &mut stop => panic!("cleanup unexpectedly finished: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(10), provider.entered.notified()) => { entered?; }
        }
    }
    assert_eq!(provider.closed.load(Ordering::Acquire), 0);
    assert!(consumer.start().await.is_err());
    provider.hold.store(false, Ordering::Release);
    consumer.stop().await?;
    assert_eq!(provider.closed.load(Ordering::Acquire), 1);
    consumer.start().await?;
    consumer.handle(batch()?).await?;
    consumer.stop().await?;
    assert_eq!(provider.closed.load(Ordering::Acquire), 2);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_weakened_restart_retains_rejected_storage_for_cleanup(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = observed(directory.path())?;
    let mut consumer = factory(plugin()?.as_ref(), sdk::ConsumerMode::Transactional)
        .create_consumer(
            id("consumer"),
            json!({}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    consumer.start().await?;
    consumer.stop().await?;
    provider.weakened.store(true, Ordering::Release);
    assert!(consumer.start().await.is_err());
    assert_eq!(provider.closed.load(Ordering::Acquire), 1);
    provider.hold.store(true, Ordering::Release);
    {
        let stop = consumer.stop();
        tokio::pin!(stop);
        tokio::select! {
            biased;
            result = &mut stop => panic!("cleanup unexpectedly finished: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(10), provider.entered.notified()) => { entered?; }
        }
    }
    assert!(consumer.start().await.is_err());
    provider.hold.store(false, Ordering::Release);
    consumer.stop().await?;
    assert_eq!(provider.closed.load(Ordering::Acquire), 2);
    provider.weakened.store(false, Ordering::Release);
    consumer.start().await?;
    consumer.handle(batch()?).await?;
    consumer.stop().await?;
    Ok(())
}
