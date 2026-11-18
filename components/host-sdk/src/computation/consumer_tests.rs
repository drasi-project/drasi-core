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

#[tokio::test]
async fn disposing_an_unstarted_consumer_releases_constructor_owned_storage() -> anyhow::Result<()>
{
    let plugin = plugin()?;
    let directory = tempfile::tempdir()?;
    let provider = provider(directory.path())?;
    let factory = factory(&plugin, sdk::ConsumerMode::Transactional);
    let component_id = ComponentId::try_new("ad3878c1-f442-4990-b613-779cb3900083")?;
    let mut first = factory
        .create_consumer(
            component_id.clone(),
            json!({"mode": "normal"}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
    first
        .initialize(&crate::computation::test_hooks::initialize_ctx())
        .await?;
    first
        .dispose(&crate::computation::test_hooks::dispose_ctx())
        .await?;
    assert!(first
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    let mut second = factory
        .create_consumer(
            component_id,
            json!({"mode": "normal"}),
            scope(),
            provider,
            options(),
        )
        .await?;
    second
        .initialize(&crate::computation::test_hooks::initialize_ctx())
        .await?;
    second
        .dispose(&crate::computation::test_hooks::dispose_ctx())
        .await?;
    Ok(())
}

#[path = "consumer_handoff_tests.rs"]
mod handoff_tests;

struct BatchSource {
    descriptor: ComponentDescriptor,
    input: Option<ChangeEnvelope>,
}
#[async_trait]
impl ComputationComponent for BatchSource {
    /// Its batch carries a restart-stable producer identity.
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::new(
            drasi_lib::computation::v1::ProgressPersistence::Durable,
        )
        .with_outputs(
            &self.descriptor,
            drasi_lib::computation::v1::OutputDescription::DURABLE,
        )
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
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
pub(in crate::computation) fn id(value: u128) -> ComponentId {
    component_id(value)
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

pub(in crate::computation) async fn initialized_consumer(
    factory: &NativeFactory,
    id: ComponentId,
    configuration: serde_json::Value,
    scope: sdk::Scope,
    provider: Arc<dyn ComputationIndexProvider>,
    options: DeliveryOptions,
) -> anyhow::Result<NativeConsumerProxy> {
    let mut consumer = factory
        .create_consumer(id, configuration, scope, provider, options)
        .await?;
    consumer
        .initialize(&crate::computation::test_hooks::initialize_ctx())
        .await?;
    Ok(consumer)
}

pub(in crate::computation) fn batch() -> anyhow::Result<InputEnvelope> {
    let source = id(77);
    let producer: GraphProducerIdentity = serde_json::from_value(json!({
        "construction_scope": GRAPH,
        "graph_id": GRAPH,
        "component_id": source.as_str(),
        "stream": "out",
        "incarnation": "c218b0c6-8803-48df-aa4c-d5350f0e6453",
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
        &super::envelope(&id(77), 1),
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
    let consumer = id(423);
    let transaction =
        ComputationTransaction::try_new(provider.create_indexes(GRAPH, &consumer).await?)?;
    let prefix: String = consumer
        .as_str()
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
    assert!(factory.create_component(id(423), json!({})).is_err());
    for mode in ["fail-second", "retry-once", "normal"] {
        let mut consumer = initialized_consumer(
            factory.as_ref(),
            id(423),
            json!({"path":destination, "mode":mode}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
        consumer
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
        let mut result = consumer.handle(batch()?).await;
        if mode == "retry-once" {
            // The transient failure crosses the ABI classified; delivering
            // the same input again, as the graph does, resumes after the
            // completed prefix with the same operation keys.
            let error = result.expect_err("transient first attempt");
            assert!(matches!(
                drasi_computation_contracts::delivery_failure_class(&error),
                Some(drasi_computation_contracts::DeliveryFailureClass::Transient { .. })
            ));
            result = consumer.handle(batch()?).await;
        }
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
        consumer
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
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
        let mut consumer = initialized_consumer(
            factory.as_ref(),
            id(423),
            json!({"mode":mode}),
            scope(),
            provider.clone(),
            options(),
        )
        .await?;
        consumer
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
        let result = consumer.handle(batch()?).await;
        if mode == "fail-second" {
            assert!(result.is_err());
        } else {
            result?;
        }
        assert_eq!(consumer.progress().await?[0].completed_operations, expected);
        consumer
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
        assert_eq!(
            count(provider.as_ref()).await?,
            Some(ElementValue::Integer(expected as i64))
        );
    }
    Ok(())
}

/// `onPermanentFailure: skip` across the ABI: a settled skip advances the
/// runner's ledger and reports the input as permanently failed (the graph
/// then acknowledges and counts it); a replay repeats nothing. A consumer
/// that settles a skip while declaring `stall`, or cannot settle under
/// `skip`, fails the node instead of leaving unfinished progress behind.
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_skip_is_settled_once_and_enforced_against_the_declaration(
) -> anyhow::Result<()> {
    use drasi_computation_contracts::lifecycle::{lifecycle_error_kind, LifecycleErrorKind};
    let plugin = plugin()?;
    let factory = factory(plugin.as_ref(), sdk::ConsumerMode::External);
    for (policy, mode, settled) in [
        ("skip", "skip-second", true),
        ("stall", "skip-second", false),
        ("skip", "fail-second", false),
    ] {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("effects.db");
        let mut consumer = initialized_consumer(
            factory.as_ref(),
            id(423),
            json!({"path": destination, "mode": mode, "onPermanentFailure": policy}),
            scope(),
            provider(&directory.path().join("progress"))?,
            options(),
        )
        .await?;
        consumer
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
        let error = consumer.handle(batch()?).await.unwrap_err();
        if settled {
            assert_eq!(
                drasi_computation_contracts::delivery_failure_class(&error),
                Some(drasi_computation_contracts::DeliveryFailureClass::Permanent)
            );
            assert!(error.chain().any(|cause| matches!(
                cause.downcast_ref::<DeliveryError>(),
                Some(DeliveryError::Skipped { skipped: 1, .. })
            )));
            let progress = consumer.progress().await?;
            assert_eq!(progress[0].completed_operations, 3);
            assert_eq!(progress[0].handled_sequence, 1);
            consumer.handle(batch()?).await?;
            assert_eq!(
                effects(&destination)?.0,
                2,
                "operations 0 and 2 applied once"
            );
        } else {
            assert_eq!(
                lifecycle_error_kind(&error),
                Some(LifecycleErrorKind::Terminal),
                "{policy}/{mode}: {error:#}"
            );
            assert!(
                consumer.handle(batch()?).await.is_err(),
                "the node is fenced"
            );
        }
        consumer
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
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
    let plugin = plugin()?;
    let factory = factory(plugin.as_ref(), sdk::ConsumerMode::Transactional);
    let mut consumer = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({"mode":"ignore-state-error"}),
        scope(),
        provider.clone(),
        options(),
    )
    .await?;
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    assert!(consumer.handle(batch()?).await.is_err());
    assert_eq!(consumer.progress().await?[0].completed_operations, 0);
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
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
    let mut consumer = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({"mode":"pending", "signal":signal}),
        scope(),
        provider.clone(),
        options(),
    )
    .await?;
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
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
    assert!(consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert_eq!(count(provider.as_ref()).await?, None);
    let mut replacement = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({}),
        scope(),
        provider.clone(),
        options(),
    )
    .await?;
    replacement
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    replacement.handle(batch()?).await?;
    replacement
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
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
    let external = factory(&plugin, sdk::ConsumerMode::External);
    let mut consumer = initialized_consumer(
        external.as_ref(),
        id(423),
        json!({"path":directory.path().join("effects.db")}),
        scope(),
        provider.clone(),
        options(),
    )
    .await?;
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    consumer.handle(batch()?).await?;
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    let transactional = factory(&plugin, sdk::ConsumerMode::Transactional);
    let mut consumer = initialized_consumer(
        transactional.as_ref(),
        id(423),
        json!({}),
        scope(),
        provider,
        options(),
    )
    .await?;
    assert!(consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
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
    let mut specification =
        factory.specification(id(423), json!({"path":directory.path().join("effects.db")}))?;
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
        .component(ComponentName::try_new("consumer")?, specification, factory)
        .source(
            ComponentName::try_new("source")?,
            Box::new(BatchSource {
                descriptor: ComponentDescriptor::try_new(
                    id(77),
                    vec![super::port(PortDirection::Output)],
                )?,
                input: Some(batch()?.envelope),
            }),
        )
        .bind_stream(
            Endpoint::new(id(77), PortId::try_new("out")?),
            StreamId::try_new("out")?,
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(id(77), PortId::try_new("out")?),
                Endpoint::new(id(423), PortId::try_new("in")?),
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

/// A native consumer's describer declares its keyed inputs; behind an
/// in-memory query the exactly-once edge fails on the consumer node while
/// both nodes stay, and `atLeastOnce` binds.
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_edges_follow_described_delivery() -> anyhow::Result<()> {
    for (configuration, compatible) in [
        (json!({}), false),
        (json!({"deliveryGuarantee": "atLeastOnce"}), true),
    ] {
        let directory = tempfile::tempdir()?;
        let factory = factory(plugin()?.as_ref(), sdk::ConsumerMode::External);
        let mut configuration = configuration;
        configuration["path"] = json!(directory.path().join("effects.db"));
        let mut specification = factory.specification(id(424), configuration)?;
        let delivery = ResourceId::try_new("delivery")?;
        specification
            .dependencies
            .insert("consumer".into(), vec![delivery.clone()]);
        let queries = Arc::new(ContinuousQueryFactory::default());
        let mut query = queries.specification_from_config(
            id(78),
            json!({"query": "MATCH (n:items) RETURN n.id AS id", "stream": "query/out"}),
        )?;
        let memory = ResourceId::try_new("memory")?;
        query
            .dependencies
            .insert("indexes".into(), vec![memory.clone()]);
        let edge = EdgeDefinition::new(
            Endpoint::new(id(78), PortId::try_new("out")?),
            Endpoint::new(id(424), PortId::try_new("in")?),
        );
        let mut graph = ComputationGraph::builder(GRAPH)
            .declare_resource(ResourceSpecification {
                id: delivery.clone(),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Borrowed,
                binding: "delivery".into(),
            })?
            .provide_resource(
                delivery,
                ResourceHandle::new(
                    ResourceRole::IndexBackend,
                    Arc::new(NativeConsumerResource {
                        provider: provider(&directory.path().join("progress"))?,
                        options: options(),
                    }),
                ),
            )?
            .declare_resource(ResourceSpecification {
                id: memory.clone(),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Borrowed,
                binding: "memory".into(),
            })?
            .provide_resource(
                memory,
                QueryIndexProviderResource::handle(
                    ResourceRole::IndexBackend,
                    Arc::new(drasi_core::computation::InMemoryComputationProvider),
                ),
            )?
            .source(
                ComponentName::try_new("feed")?,
                Box::new(BatchSource {
                    descriptor: ComponentDescriptor::try_new(
                        id(77),
                        vec![super::port(PortDirection::Output)],
                    )?,
                    input: None,
                }),
            )
            .bind_stream(
                Endpoint::new(id(77), PortId::try_new("out")?),
                StreamId::try_new("feed/out")?,
            )
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(id(77), PortId::try_new("out")?),
                    Endpoint::new(id(78), PortId::try_new("in")?),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .component(ComponentName::try_new("query")?, query, queries)
            .bind_stream(edge.from.clone(), StreamId::try_new("query/out")?)
            .component(ComponentName::try_new("consumer")?, specification, factory)
            .connect(edge.clone(), Box::new(BoundedPipeConfig { capacity: 1 }))
            .build()?;
        let run = graph.run()?;
        let control = run.control();
        let (outcome, observed) = tokio::join!(run, async {
            control.deployment_report().await?;
            let mut changes = control.subscribe_observed();
            tokio::time::timeout(
                Duration::from_secs(10),
                changes.wait_for(|state| {
                    matches!(
                        state.relationships[&edge].binding,
                        BindingState::Bound | BindingState::Failed
                    )
                }),
            )
            .await??;
            let observed = control.observed();
            control.cancel();
            anyhow::Ok(observed)
        });
        assert!(matches!(outcome, Err(GraphError::Cancelled)), "{outcome:?}");
        graph.dispose().await?;
        let observed = observed?;
        let consumer = &observed.components[&id(424)];
        if compatible {
            assert_eq!(observed.relationships[&edge].binding, BindingState::Bound);
            assert!(consumer.failure.is_none(), "{consumer:?}");
        } else {
            assert_eq!(observed.relationships[&edge].binding, BindingState::Failed);
            let failure = consumer.failure.as_ref().expect("binding failure");
            assert_eq!(failure.phase, FailurePhase::Binding);
            assert_eq!(failure.disposition, FailureDisposition::Terminal);
            assert!(observed.components[&id(78)].failure.is_none());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_panic_requires_reconstruction_after_cleanup() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let factory = factory(plugin()?.as_ref(), sdk::ConsumerMode::Transactional);
    let provider = provider(directory.path())?;
    let mut consumer = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({"mode":"panic"}),
        scope(),
        provider.clone(),
        options(),
    )
    .await?;
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    assert!(consumer.handle(batch()?).await.is_err());
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert!(consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert_eq!(count(provider.as_ref()).await?, None);
    let mut replacement = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({}),
        scope(),
        provider,
        options(),
    )
    .await?;
    replacement
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    replacement.handle(batch()?).await?;
    replacement
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
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
    metadata.capabilities.snapshot_inputs = vec![metadata.ports[0].id().clone()];
    assert!(sdk::ConsumerMode::External
        .validate_factory(&metadata)
        .is_err());
    metadata.capabilities.snapshot_inputs.clear();
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
        let factory = factory(&plugin, sdk::ConsumerMode::Transactional);
        let mut sink = initialized_consumer(
            factory.as_ref(),
            id(423),
            json!({}),
            scope,
            resource.provider.clone(),
            resource.options.clone(),
        )
        .await?;
        sink.start(&crate::computation::test_hooks::start_ctx())
            .await?;
        sink.handle(batch()?).await?;
        sink.stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
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
        component: &drasi_core_types::ComponentId,
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
        .create_consumer(id(423), json!({}), scope(), provider.clone(), unsupported,)
        .await
        .is_err());
    assert_eq!(provider.closed.load(Ordering::Acquire), 1);
    let mut consumer = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({}),
        scope(),
        provider.clone(),
        options(),
    )
    .await?;
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert_eq!(provider.closed.load(Ordering::Acquire), 2);
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
async fn native_consumer_cancelled_stop_retains_storage_until_cleanup_completes(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = observed(directory.path())?;
    let plugin = plugin()?;
    let factory = factory(plugin.as_ref(), sdk::ConsumerMode::Transactional);
    let mut consumer = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({}),
        scope(),
        provider.clone(),
        options(),
    )
    .await?;
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    provider.hold.store(true, Ordering::Release);
    {
        let stop_context = crate::computation::test_hooks::stop_ctx();
        let stop = consumer.stop(&stop_context);
        tokio::pin!(stop);
        tokio::select! {
            result = &mut stop => panic!("cleanup unexpectedly finished: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(10), provider.entered.notified()) => { entered?; }
        }
    }
    assert_eq!(provider.closed.load(Ordering::Acquire), 0);
    assert!(consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    provider.hold.store(false, Ordering::Release);
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert_eq!(provider.closed.load(Ordering::Acquire), 1);
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    consumer.handle(batch()?).await?;
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert_eq!(provider.closed.load(Ordering::Acquire), 2);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_weakened_restart_retains_rejected_storage_for_cleanup(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = observed(directory.path())?;
    let plugin = plugin()?;
    let factory = factory(plugin.as_ref(), sdk::ConsumerMode::Transactional);
    let mut consumer = initialized_consumer(
        factory.as_ref(),
        id(423),
        json!({}),
        scope(),
        provider.clone(),
        options(),
    )
    .await?;
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    provider.weakened.store(true, Ordering::Release);
    assert!(consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    assert_eq!(provider.closed.load(Ordering::Acquire), 1);
    provider.hold.store(true, Ordering::Release);
    {
        let stop_context = crate::computation::test_hooks::stop_ctx();
        let stop = consumer.stop(&stop_context);
        tokio::pin!(stop);
        tokio::select! {
            biased;
            result = &mut stop => panic!("cleanup unexpectedly finished: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(10), provider.entered.notified()) => { entered?; }
        }
    }
    assert!(consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    provider.hold.store(false, Ordering::Release);
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert_eq!(provider.closed.load(Ordering::Acquire), 2);
    provider.weakened.store(false, Ordering::Release);
    consumer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    consumer.handle(batch()?).await?;
    consumer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    Ok(())
}

/// One live output of an in-memory query run: no persistent progress, a
/// per-run incarnation in its recovery identity.
fn volatile_batch(incarnation: &str) -> anyhow::Result<InputEnvelope> {
    use drasi_core_types::evaluation::{
        context::QueryPartEvaluationContext, variable_value::VariableValue,
    };
    let query = id(78);
    let mut envelope = QueryChangeCodec::encode_evaluation(
        None,
        &query,
        SystemMetadata::new(StreamId::try_new("query/out")?, 1),
        &ResultNodeProjector::try_new("test/native-consumer-volatile/out", None)?,
        &(0..3)
            .map(|index| QueryPartEvaluationContext::Adding {
                after: [("value".into(), VariableValue::Integer(index.into()))].into(),
                row_signature: index as u64 + 1,
            })
            .collect::<Vec<_>>(),
        QueryOutputMetadata {
            query_id: query.clone(),
            input_origin: None,
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0)
                .ok_or_else(|| anyhow::anyhow!("timestamp"))?,
            metadata: Default::default(),
            profiling: None,
        },
    )?
    .ok_or_else(|| anyhow::anyhow!("volatile output"))?;
    let identity: QueryRecoveryIdentity = serde_json::from_value(json!({
        "construction_scope": GRAPH, "graph_id": GRAPH, "query_id": query.as_str(),
        "configuration_hash": 7, "incarnation": incarnation
    }))?;
    identity.annotate(&mut envelope, &query)?;
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn native_consumer_delivers_volatile_queries_only_at_least_once_by_opt_in(
) -> anyhow::Result<()> {
    const RUN: &str = "0b7f6f0e-8a33-4b1e-9d0c-6c3f2a1e5d01";
    let directory = tempfile::tempdir()?;
    let plugin = plugin()?;
    let factory = factory(plugin.as_ref(), sdk::ConsumerMode::External);
    let destination = directory.path().join("effects.db");
    // Declared exactly-once: an envelope without a durable producer identity
    // is a permanent, typed failure, never a silent at-least-once downgrade
    // (SPA-7). Nothing was opened, so no cleanup is required.
    let mut strict = initialized_consumer(
        factory.as_ref(),
        id(431),
        json!({"path": destination}),
        scope(),
        provider(&directory.path().join("strict"))?,
        options(),
    )
    .await?;
    strict
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    let error = strict
        .handle(volatile_batch(RUN)?)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("the volatile input must be refused"))?;
    assert_eq!(
        drasi_computation_contracts::delivery_failure_class(&error),
        Some(drasi_computation_contracts::DeliveryFailureClass::Permanent),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("lacks the producer identity that ExactlyOnce"),
        "{error:#}"
    );
    strict.handle(batch()?).await?;
    strict
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert_eq!(
        effects(&destination)?,
        (3, 3, 3),
        "only the persistent batch ran"
    );

    // Declared at-least-once: every input is delivered without the host
    // ledger, in order, whatever identity the producer carries.
    let opted_destination = directory.path().join("opted.db");
    let mut opted = initialized_consumer(
        factory.as_ref(),
        id(432),
        json!({"path": opted_destination, "deliveryGuarantee": "atLeastOnce"}),
        scope(),
        provider(&directory.path().join("opted"))?,
        options(),
    )
    .await?;
    opted
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    opted.handle(volatile_batch(RUN)?).await?;
    opted.handle(volatile_batch(RUN)?).await?;
    assert!(opted.progress().await?.is_empty(), "no host-local ledger");
    // The declared guarantee alone selects the mode: a durable producer's
    // input is also delivered at least once, deduplicated by the destination.
    opted.handle(batch()?).await?;
    assert!(opted.progress().await?.is_empty(), "no host-local ledger");
    opted
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    // At least once: the retry reached the plugin with the same per-run
    // operation identities (9 attempts, 6 distinct effects).
    assert_eq!(effects(&opted_destination)?, (6, 9, 6));
    Ok(())
}

#[test]
fn native_consumer_factories_describe_their_keyed_inputs() -> anyhow::Result<()> {
    use drasi_lib::computation::v1::InputDelivery;
    let plugin = plugin()?;
    let external = factory(plugin.as_ref(), sdk::ConsumerMode::External);
    let transactional = factory(plugin.as_ref(), sdk::ConsumerMode::Transactional);
    let describe = |factory: &NativeFactory, configuration: serde_json::Value| {
        let specification = factory.specification(id(433), configuration.clone())?;
        let request = sdk::CreateRequest {
            id: id(433),
            implementation: specification.implementation.clone(),
            configuration_version: specification.configuration_version,
            configuration,
            scope: None,
            inputs: Vec::new(),
            activation_credentials_bound: false,
            update_directive: None,
        };
        let description =
            factory.describe_request(&request, &specification.descriptor, Default::default())?;
        anyhow::Ok(description.input(&PortId::try_new("in")?).delivery)
    };
    for (configuration, delivery) in [
        (json!({}), InputDelivery::ExactlyOnce),
        (
            json!({"deliveryGuarantee": "exactlyOnce"}),
            InputDelivery::ExactlyOnce,
        ),
        (
            json!({"deliveryGuarantee": "atLeastOnce"}),
            InputDelivery::AtLeastOnce,
        ),
    ] {
        assert_eq!(
            describe(&external, configuration.clone())?,
            delivery,
            "{configuration}"
        );
        assert_eq!(
            describe(&transactional, configuration)?,
            InputDelivery::ExactlyOnce
        );
    }
    Ok(())
}
