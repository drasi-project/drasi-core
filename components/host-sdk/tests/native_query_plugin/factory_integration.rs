// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use futures_util::StreamExt;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

struct LiveSource {
    descriptor: ComponentDescriptor,
    incoming: Arc<tokio::sync::Mutex<mpsc::Receiver<OutputEnvelope>>>,
}

#[async_trait]
impl ComputationComponent for LiveSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for LiveSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.incoming.lock().await.recv().await)
    }
}

struct LiveSourceFactory {
    descriptor: FactoryDescriptor,
    incoming: Arc<tokio::sync::Mutex<mpsc::Receiver<OutputEnvelope>>>,
}

#[async_trait]
impl ComponentFactory for LiveSourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, _: &ComponentSpecification) -> Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        Ok(ConstructedComponent::source(Box::new(LiveSource {
            descriptor: context.specification.descriptor.clone(),
            incoming: self.incoming.clone(),
        })))
    }
}

fn source() -> Result<(ComponentBatch, mpsc::Sender<OutputEnvelope>)> {
    let (sender, receiver) = mpsc::channel(8);
    let factory = Arc::new(LiveSourceFactory {
        descriptor: FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("test/query-plugin-input", "1")?,
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: ConfigurationSchema::default(),
            dependencies: BTreeMap::new(),
        },
        incoming: Arc::new(tokio::sync::Mutex::new(receiver)),
    });
    let id = ComponentId::try_new("source")?;
    let batch = ComponentBatch::builder()
        .component(
            ComponentSpecification {
                descriptor: ComponentDescriptor::try_new(
                    id.clone(),
                    vec![port("out", PortDirection::Output)?],
                )?,
                role: ComponentRole::Source,
                completion: None,
                implementation: factory.descriptor.implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::new(),
                dependencies: BTreeMap::new(),
            },
            factory,
        )
        .bind_stream(
            Endpoint::new(id.clone(), PortId::try_new("out")?),
            StreamId::try_new("source/out")?,
        )
        .lifecycle_policy(id, LifecyclePolicy { auto_start: true })
        .build()?;
    Ok((batch, sender))
}

async fn emit(sender: &mpsc::Sender<OutputEnvelope>, value: i64, sequence: u64) -> Result<()> {
    sender
        .send(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: input(
                SourceChange::Replace {
                    element: item(value, sequence),
                },
                sequence,
            )?
            .envelope,
        })
        .await?;
    Ok(())
}

fn builder(
    path: &Path,
    native: Arc<dyn ComponentFactory>,
    volatile: bool,
) -> drasi_lib::DrasiLibBuilder {
    let builder = drasi_lib::DrasiLib::builder()
        .with_id(GRAPH)
        .with_query_factory(native);
    if volatile {
        builder
    } else {
        builder.with_default_index_provider(
            "persistent",
            Arc::new(drasi_index_rocksdb::RocksDbIndexProvider::new(
                path, false, false,
            )),
        )
    }
}

async fn ordinary_factory(volatile: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let native = factory()?.for_ordinary_queries()?;
    let implementation = native.descriptor().implementation.clone();
    let (source, sender) = source()?;
    let mut config = drasi_lib::Query::cypher(COMPONENT)
        .query("MATCH (n:NativeItem) RETURN sum(n.value) AS total")
        .from_input_with_pipeline("source", vec!["labels".into()])
        .with_middleware(drasi_core::models::SourceMiddlewareConfig::new(
            "relabel",
            "labels",
            json!({"labelMappings": {"Item": "NativeItem"}})
                .as_object()
                .context("middleware object")?
                .clone(),
        ))
        .enable_bootstrap(false)
        .with_outbox_capacity(8)
        .build();
    config.priority_queue_capacity = Some(16);
    config.dispatch_buffer_capacity = Some(16);
    let core = builder(directory.path(), native, volatile)
        .with_query(config.clone())
        .with_components(source)
        .build()
        .await?;
    let run = async {
        anyhow::ensure!(
            serde_json::to_value(core.get_query_config(COMPONENT).await?)?
                == serde_json::to_value(&config)?,
            "ordinary query configuration changed during native construction"
        );
        anyhow::ensure!(
            core.computation_control()?
                .desired_snapshot()
                .specifications[&ComponentId::try_new(COMPONENT)?]
                .implementation
                == implementation,
            "ordinary creation selected a different query factory"
        );
        core.start().await?;
        let mut subscription = core.subscribe_query_results(COMPONENT).await?.stream;
        emit(&sender, 5, 1).await?;
        tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await?
            .context("query subscription closed")??;
        let rows = core.get_query_results(COMPONENT).await?;
        anyhow::ensure!(
            rows.len() == 1 && rows[0]["total"].as_f64() == Some(5.0),
            "unexpected native query results: {rows:?}"
        );
        core.stop_query(COMPONENT).await?;
        core.start_query(COMPONENT).await?;
        emit(&sender, 7, 2).await?;
        tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await?
            .context("restart closed query subscription")??;
        let rows = core.get_query_results(COMPONENT).await?;
        anyhow::ensure!(
            rows.len() == 1 && rows[0]["total"].as_f64() == Some(7.0),
            "native query did not retain its indexed state across restart: {rows:?}"
        );
        let query = core.query_manager().get_query_instance(COMPONENT).await?;
        anyhow::ensure!(
            query.fetch_outbox(0).await?.results.len() == 2,
            "ordinary query lost native retained output"
        );
        query.fetch_snapshot().await?;
        let metrics = query
            .output_metrics()
            .context("native query output metrics")?
            .snapshot();
        assert_eq!(metrics.result_seq_advances, 2);
        assert_eq!(metrics.live_results_count, 1);
        assert_eq!(metrics.outbox_size, 2);
        assert_eq!(metrics.outbox_earliest_seq, 1);
        assert_eq!(metrics.outbox_latest_seq, 2);
        assert!(metrics.snapshot_fetch_count > 0);
        assert!(metrics.outer_transaction_duration_ns_last > 0);
        assert!(
            metrics.outer_transaction_duration_ns_max >= metrics.outer_transaction_duration_ns_last
        );
        let mut later = config.clone();
        later.id = "later-query".into();
        core.add_query(later).await?;
        anyhow::ensure!(
            core.computation_control()?
                .desired_snapshot()
                .specifications[&ComponentId::try_new("later-query")?]
                .implementation
                == implementation,
            "a query added after startup did not use the selected native factory"
        );
        let mut later = core.subscribe_query_results("later-query").await?.stream;
        emit(&sender, 9, 3).await?;
        tokio::time::timeout(Duration::from_secs(5), later.next())
            .await?
            .context("late query subscription closed")??;
        let rows = core.get_query_results("later-query").await?;
        anyhow::ensure!(
            rows.len() == 1 && rows[0]["total"].as_f64() == Some(9.0),
            "late native query did not process source changes: {rows:?}"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let shutdown = core.shutdown().await;
    run?;
    shutdown?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_native_queries_transfer_large_results_and_retained_history() -> Result<()> {
    const BATCHES: usize = 1025;
    const ROWS_PER_BATCH: usize = 17;
    let directory = tempfile::tempdir()?;
    let (source, sender) = source()?;
    let core = builder(directory.path(), factory()?.for_ordinary_queries()?, true)
        .with_query(
            drasi_lib::Query::cypher(COMPONENT)
                .query("MATCH (n:Item) RETURN n.value AS id, n.payload AS payload")
                .from_input("source")
                .enable_bootstrap(false)
                .with_outbox_capacity(BATCHES)
                .build(),
        )
        .with_components(source)
        .build()
        .await?;
    let run = tokio::time::timeout(Duration::from_secs(120), async {
        core.start().await?;
        let mut subscription = core.subscribe_query_results(COMPONENT).await?.stream;
        let send = async {
            let payload = "x".repeat(1024);
            for batch in 0..BATCHES {
                let sequence = u64::try_from(batch + 1)?;
                let changes: Vec<_> = (0..ROWS_PER_BATCH)
                    .map(|offset| {
                        let id = batch * ROWS_PER_BATCH + offset;
                        SourceChange::Insert {
                            element: Element::Node {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new("source", &id.to_string()),
                                    labels: Arc::from([Arc::from("Item")]),
                                    effective_from: sequence,
                                },
                                properties: ElementPropertyMap::from(json!({
                                    "value": id,
                                    "payload": payload,
                                })),
                            },
                        }
                    })
                    .collect();
                sender
                    .send(OutputEnvelope {
                        port: PortId::try_new("out")?,
                        envelope: GraphChangeCodec::encode_changes(
                            &changes,
                            StreamId::try_new("source/out")?,
                            sequence,
                            None,
                        )?,
                    })
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let consume = async {
            for _ in 0..BATCHES {
                subscription
                    .next()
                    .await
                    .context("native result stream closed")??;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(send, consume)?;
        let rows = core.get_query_results(COMPONENT).await?;
        assert_eq!(rows.len(), BATCHES * ROWS_PER_BATCH);
        assert!(rows.len() > drasi_computation_plugin_abi::output_reads::MAX_SNAPSHOT_RECORDS);
        let mut ids = std::collections::BTreeSet::new();
        let mut bytes = 0;
        for row in rows {
            assert!(ids.insert(row["id"].as_u64().context("numeric result id")?));
            let payload = row["payload"].as_str().context("result payload")?;
            assert_eq!(payload, "x".repeat(1024));
            bytes += payload.len();
        }
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            (0..u64::try_from(BATCHES * ROWS_PER_BATCH)?).collect::<Vec<_>>()
        );
        assert!(bytes > drasi_computation_plugin_abi::output_reads::MAX_READ_BYTES);
        let query = core.query_manager().get_query_instance(COMPONENT).await?;
        assert_eq!(query.fetch_outbox(0).await?.results.len(), BATCHES);
        let _late_subscription = core.subscribe_query_results(COMPONENT).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let shutdown = core.shutdown().await;
    run??;
    shutdown?;
    Ok(())
}

async fn ordinary_non_atomic() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (source, sender) = source()?;
    let core = builder(directory.path(), factory()?.for_ordinary_queries()?, true)
        .with_components(source)
        .build()
        .await?;
    let work = async {
        let query = drasi_lib::Query::cypher(COMPONENT)
            .query(QUERY)
            .from_input("source")
            .enable_bootstrap(false)
            .build();
        let pipeline = core
            .computation_pipeline()?
            .native_source(Endpoint::new(
                ComponentId::try_new("source")?,
                PortId::try_new("out")?,
            ))?
            .query(query)
            .query_provider(
                COMPONENT,
                Arc::new(super::non_atomic_store::SplitOutputProvider::new(
                    directory.path(),
                )?),
                QueryPublicationMode::NonAtomic,
            )?
            .build()?;
        assert!(core.add_components(pipeline).await?.committed);
        core.start().await?;
        assert_eq!(
            core.computation_control()?
                .desired_snapshot()
                .specifications[&ComponentId::try_new(COMPONENT)?]
                .implementation
                .name
                .as_ref(),
            "drasi.experimental/indexed-query/ordinary",
        );
        let mut subscription = core.subscribe_query_results(COMPONENT).await?.stream;
        emit(&sender, 5, 1).await?;
        tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await?
            .context("query stream")??;
        assert_eq!(
            core.get_query_results(COMPONENT).await?[0]["total"].as_f64(),
            Some(5.0)
        );
        core.stop_query(COMPONENT).await?;
        core.start_query(COMPONENT).await?;
        emit(&sender, 7, 2).await?;
        tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await?
            .context("restarted query stream")??;
        assert_eq!(
            core.get_query_results(COMPONENT).await?[0]["total"].as_f64(),
            Some(7.0)
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let shutdown = core.shutdown().await;
    work?;
    shutdown?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_non_atomic_query_uses_the_registered_plugin_current_thread() -> Result<()> {
    ordinary_non_atomic().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_non_atomic_query_uses_the_registered_plugin_multi_thread() -> Result<()> {
    ordinary_non_atomic().await
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_queries_use_selected_native_factory_current_thread() -> Result<()> {
    ordinary_factory(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_queries_use_selected_native_factory_multi_thread() -> Result<()> {
    ordinary_factory(false).await
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_queries_use_native_memory_factory_current_thread() -> Result<()> {
    ordinary_factory(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_queries_use_native_memory_factory_multi_thread() -> Result<()> {
    ordinary_factory(true).await
}

#[cfg(feature = "test-support")]
async fn ordinary_clock(volatile: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let native = factory()?.for_ordinary_queries()?;
    let (source, sender) = source()?;
    let clock = drasi_lib::test_support::QueryTestControl::new(1000);
    let config = drasi_lib::Query::cypher(COMPONENT)
        .query(super::scheduling::TEMPORAL)
        .from_input("source")
        .enable_bootstrap(false)
        .with_outbox_capacity(8)
        .build();
    let core = builder(directory.path(), native, volatile)
        .with_query_test_control(COMPONENT, clock.clone())
        .with_query(config)
        .with_components(source)
        .build()
        .await?;
    let run = async {
        core.start().await?;
        let mut subscription = core.subscribe_query_results(COMPONENT).await?.stream;
        sender
            .send(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: super::scheduling::scheduled_input(1000, 2000)?.envelope,
            })
            .await?;
        tokio::time::timeout(Duration::from_secs(5), subscription.next())
            .await?
            .context("temporal query subscription closed")??;
        let before = clock.advance_to(1999, Duration::from_secs(5)).await?;
        anyhow::ensure!(
            before.physical_time_ms == 1999
                && before.output_sequence == 1
                && before.output_persistent != volatile
                && before.publication
                    == if volatile {
                        QueryPublicationMode::NonAtomic
                    } else {
                        QueryPublicationMode::Atomic
                    },
            "native query used the wrong clock or frontier: {before:?}"
        );
        let due = clock.advance_to(2000, Duration::from_secs(5)).await?;
        anyhow::ensure!(
            due.output_sequence == 2 && due.output_generation == before.output_generation,
            "native query did not drain its actual due work: {due:?}"
        );
        let rows = core.get_query_results(COMPONENT).await?;
        anyhow::ensure!(
            rows.len() == 1 && rows[0]["expired"] == true,
            "missing expiry result: {rows:?}"
        );
        core.stop_query(COMPONENT).await?;
        core.start_query(COMPONENT).await?;
        let reopened = clock.wake(Duration::from_secs(5)).await?;
        anyhow::ensure!(
            reopened.physical_time_ms == 2000
                && reopened.output_sequence == 2
                && reopened.output_generation == due.output_generation,
            "native clock or output frontier changed across restart: {reopened:?}"
        );
        core.remove_query(COMPONENT).await?;
        anyhow::ensure!(
            clock.wake(Duration::from_secs(5)).await.is_err(),
            "removed query retained a live test-control endpoint"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let shutdown = core.shutdown().await;
    run?;
    shutdown?;
    Ok(())
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "current_thread")]
async fn ordinary_native_query_uses_existing_test_clock_current_thread() -> Result<()> {
    ordinary_clock(false).await
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_native_query_uses_existing_test_clock_multi_thread() -> Result<()> {
    ordinary_clock(false).await
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "current_thread")]
async fn ordinary_native_memory_query_uses_existing_test_clock_current_thread() -> Result<()> {
    ordinary_clock(true).await
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_native_memory_query_uses_existing_test_clock_multi_thread() -> Result<()> {
    ordinary_clock(true).await
}
