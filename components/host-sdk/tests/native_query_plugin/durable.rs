// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::Scope;
use drasi_host_sdk::computation::{NativeComponentProxy, NativeFactory};
use std::path::Path;

#[path = "archives.rs"]
mod archives;
#[path = "durable_bootstrap.rs"]
mod bootstrap;
#[path = "corrupt_recovery.rs"]
mod corrupt_recovery;
#[path = "durable_destinations.rs"]
mod destinations;
#[path = "input_middleware.rs"]
mod input_middleware;
#[path = "non_atomic.rs"]
mod non_atomic;
#[path = "non_atomic_store.rs"]
mod non_atomic_store;
#[path = "factory_integration.rs"]
mod ordinary;
#[path = "durable_progress.rs"]
mod progress;
#[path = "retained_recovery.rs"]
mod retained_recovery;
#[path = "scheduling.rs"]
mod scheduling;
#[path = "shared_output.rs"]
mod shared_output;
#[path = "durable_snapshots.rs"]
mod snapshots;

const QUERY: &str = "MATCH (n:Item) RETURN sum(n.value) AS total";
const COMPONENT: &str = "durable-query";
const COMPONENT_ID: &str = "3283a204-e18e-5bf6-8246-51c299f70b09";
const GRAPH: &str = "native-query-durability";
const CRASH_ROOT: &str = "DRASI_QUERY_PLUGIN_DURABILITY_ROOT";

fn component_id() -> Result<ComponentId> {
    Ok(ComponentId::try_new(COMPONENT_ID)?)
}

fn component_name() -> Result<ComponentName> {
    Ok(ComponentName::try_new(COMPONENT)?)
}

fn output_partition() -> Result<drasi_core::interface::OutputPartition> {
    Ok(drasi_core::interface::OutputPartition::Component(
        component_id()?,
    ))
}

fn metadata_checkpoint(name: &str) -> drasi_core::interface::CheckpointKey {
    drasi_core::interface::CheckpointKey::Metadata(name.to_owned())
}

fn factory() -> Result<Arc<NativeFactory>> {
    let plugin = computation::load(library())?;
    plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .cloned()
        .context("indexed query factory missing")
}

fn provider(path: &Path) -> Result<Arc<RocksDbComputationProvider>> {
    Ok(Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    )))
}

fn scope(generation: u64) -> Scope {
    Scope {
        instance_id: GRAPH.into(),
        graph_id: GRAPH.into(),
        generation,
    }
}

fn create(path: &Path, text: &str, generation: u64) -> Result<NativeComponentProxy> {
    factory()?.create_component_with_processing(
        component_id()?,
        json!({"query":text, "stream":format!("{COMPONENT}/out")}),
        scope(generation),
        provider(path)?,
    )
}

fn aggregate(
    outputs: &[OutputEnvelope],
    before: Option<f64>,
    after: f64,
    sequence: u64,
) -> Result<()> {
    let [output] = outputs else {
        anyhow::bail!("expected one aggregate output");
    };
    assert_eq!(
        QueryChangeCodec::query_sequence(&output.envelope)?,
        sequence
    );
    let progress = GraphProducerProgress::from_envelope(&output.envelope)?
        .context("durable producer progress")?;
    assert!(progress.identity().persistent());
    assert_eq!(progress.sequence(), sequence);
    match QueryChangeCodec::decode_evaluation(&output.envelope)?.as_slice() {
        [QueryPartEvaluationContext::Aggregation {
            before: actual_before,
            after: actual_after,
            ..
        }] => {
            assert_eq!(
                actual_before
                    .as_ref()
                    .and_then(|values| values.get("total"))
                    .cloned(),
                before.map(|value| VariableValue::from(json!(value)))
            );
            assert_eq!(
                actual_after.get("total"),
                Some(&VariableValue::from(json!(after)))
            );
        }
        other => anyhow::bail!("unexpected aggregate result: {other:?}"),
    }
    Ok(())
}

async fn restart_receipt() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path();
    let original = input(
        SourceChange::Insert {
            element: item(10, 1),
        },
        1,
    )?;
    let mut query = create(path, QUERY, 1)?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.has_pending_emissions());
    assert!(query.continue_transform().await?.is_empty());
    assert!(!query.has_pending_emissions());
    let first = query.transform(original.clone()).await?;
    aggregate(&first, Some(0.0), 10.0, 1)?;
    let producer =
        GraphProducerProgress::from_envelope(&first[0].envelope)?.context("first producer")?;
    query.stop().await?;
    drop(query);

    let mut query = create(path, QUERY, 2)?;
    query.initialize().await?;
    query.start().await?;
    assert!(
        query.has_pending_emissions(),
        "startup must expose pending replay before input"
    );
    let replayed = query.continue_transform().await?;
    aggregate(&replayed, Some(0.0), 10.0, 1)?;
    assert_eq!(
        GraphProducerProgress::from_envelope(&replayed[0].envelope)?.as_ref(),
        Some(&producer)
    );
    assert_eq!(
        replayed[0].envelope.changes().operations(),
        first[0].envelope.changes().operations()
    );
    assert!(replayed[0].envelope.system().sequence() > first[0].envelope.system().sequence());
    query.delivery_completed(&replayed).await?;
    query.delivery_completed(&replayed).await?;
    assert!(
        query.transform(original).await?.is_empty(),
        "committed input must not be evaluated again"
    );
    let updated = query
        .transform(input(
            SourceChange::Replace {
                element: item(12, 2),
            },
            2,
        )?)
        .await?;
    aggregate(&updated, Some(10.0), 12.0, 2)?;
    assert_eq!(updated[0].envelope.system().sequence(), 3);
    query.delivery_completed(&updated).await?;
    query.stop().await?;
    drop(query);

    let mut query = create(path, QUERY, 3)?;
    query.initialize().await?;
    query.start().await?;
    assert!(
        query.continue_transform().await?.is_empty(),
        "confirmed output must not replay"
    );
    let next = query
        .transform(input(
            SourceChange::Insert {
                element: item_with_id("two", 3, 3),
            },
            3,
        )?)
        .await?;
    aggregate(&next, Some(12.0), 15.0, 3)?;
    assert_eq!(next[0].envelope.system().sequence(), 4);
    query.delivery_completed(&next).await?;
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_state_progress_and_pending_output_survive_reopen_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), restart_receipt()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_state_progress_and_pending_output_survive_reopen_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), restart_receipt()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_deprovision_clears_owned_rows_progress_and_bootstrap_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut query = create(directory.path(), QUERY, 1)?;
    query.initialize().await?;
    query.start().await?;
    query
        .bootstrap_state()
        .expect("bootstrap state")
        .write(b"owned-test-intent".to_vec().into())
        .await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(10, 1),
            },
            1,
        )?)
        .await?;
    let producer = GraphProducerProgress::from_envelope(&output[0].envelope)?
        .expect("committed producer progress")
        .identity()
        .clone();
    assert!(
        query.delete_owned_resources().await.is_err(),
        "running work cannot be deleted"
    );
    query.stop().await?;
    query.delete_owned_resources().await?;
    query.delete_owned_resources().await?;
    assert!(
        query.start().await.is_err(),
        "a deleted instance cannot reactivate"
    );
    drop(query);
    let mut recreated = create(directory.path(), QUERY, 2)?;
    recreated.initialize().await?;
    recreated.start().await?;
    assert!(recreated
        .bootstrap_state()
        .expect("bootstrap state")
        .read()
        .await?
        .is_none());
    assert!(recreated.continue_transform().await?.is_empty());
    let progress = recreated
        .processing_progress_owner()
        .expect("actual progress owner")
        .snapshot();
    assert!(progress.checkpoints.is_empty());
    assert_eq!(progress.reset_generation, 1);
    let output = recreated
        .transform(input(
            SourceChange::Insert {
                element: item(3, 1),
            },
            1,
        )?)
        .await?;
    aggregate(&output, Some(0.0), 3.0, 1)?;
    assert_ne!(
        GraphProducerProgress::from_envelope(&output[0].envelope)?
            .expect("committed producer progress")
            .identity(),
        &producer
    );
    recreated.delivery_completed(&output).await?;
    recreated.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_preparation_exposes_actual_recovery_evidence_before_activation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut query = create(directory.path(), QUERY, 1)?;
    let unprepared = query.recovery_contract();
    query.initialize().await?;
    query.prepare_processing().await?;
    let prepared = query.recovery_contract();
    assert_ne!(prepared, unprepared);
    assert!(
        !query
            .processing_progress_owner()
            .expect("actual progress owner")
            .snapshot()
            .recovered
    );
    assert!(
        query
            .bootstrap_state()
            .expect("bootstrap state")
            .read()
            .await
            .is_err(),
        "preparation admitted indexed work before activation"
    );
    query.start().await?;
    assert_eq!(query.recovery_contract(), prepared);
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(10, 1),
            },
            1,
        )?)
        .await?;
    aggregate(&output, Some(0.0), 10.0, 1)?;
    query.delivery_completed(&output).await?;
    query.stop().await?;
    query.start().await?;
    assert_eq!(query.recovery_contract(), prepared);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_reopen_rejects_changed_configuration_without_erasing_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path();
    let mut query = create(path, QUERY, 1)?;
    query.initialize().await?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(10, 1),
            },
            1,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    query.stop().await?;
    drop(query);
    let mut changed = create(path, "MATCH (n:Item) RETURN count(n) AS total", 2)?;
    changed.initialize().await?;
    let error = changed
        .start()
        .await
        .expect_err("configuration mismatch must not reuse indexes");
    assert!(
        format!("{error:#}").contains("configuration changed"),
        "{error:#}"
    );
    changed.stop().await?;
    drop(changed);
    let mut query = create(path, QUERY, 3)?;
    query.initialize().await?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: item(12, 2),
            },
            2,
        )?)
        .await?;
    aggregate(&output, Some(10.0), 12.0, 2)?;
    query.delivery_completed(&output).await?;
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_confirmation_cannot_discard_a_different_pending_output() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut query = create(directory.path(), QUERY, 1)?;
    query.initialize().await?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(10, 1),
            },
            1,
        )?)
        .await?;
    let wrong = vec![OutputEnvelope {
        port: output[0].port.clone(),
        envelope: output[0].envelope.reemit(999)?,
    }];
    assert!(query.delivery_completed(&wrong).await.is_err());
    query.stop().await?;
    drop(query);
    let mut query = create(directory.path(), QUERY, 2)?;
    query.initialize().await?;
    query.start().await?;
    let progress = query
        .committed_input_progress()?
        .context("reopened progress view")?;
    let committed = progress.wait_ready().await?;
    assert!(committed.persistent);
    assert_eq!(
        committed.checkpoints[&SourceProgressKey::Stream(StreamId::try_new("source/out")?)]
            .sequence,
        1
    );
    let output = query.continue_transform().await?;
    aggregate(&output, Some(0.0), 10.0, 1)?;
    query.delivery_completed(&output).await?;
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_persistence_rejects_unqualified_storage() -> Result<()> {
    let mut query = factory()?.create_component_with_processing(
        component_id()?,
        json!({"query":QUERY}),
        scope(1),
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )?;
    query.initialize().await?;
    assert!(query.start().await.is_err());
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "process-exit fixture launched by the durable graph cases"]
async fn indexed_commit_exit_child() -> Result<()> {
    let path =
        PathBuf::from(std::env::var_os(CRASH_ROOT).context("durability child needs a directory")?);
    if let Some(phase) = std::env::var_os(non_atomic::CRASH_PHASE) {
        return non_atomic::exit_child(&path, phase.to_str().context("invalid crash phase")?).await;
    }
    let mut query = create(&path, QUERY, 1)?;
    query.initialize().await?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(10, 1),
            },
            1,
        )?)
        .await?;
    aggregate(&output, Some(0.0), 10.0, 1)?;
    // The host committed, but neither delivery confirmation nor cleanup runs.
    std::process::exit(86)
}

async fn process_exit_and_graph_replay() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "durable::indexed_commit_exit_child",
                "--ignored",
                "--nocapture",
            ])
            .env(CRASH_ROOT, directory.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("durable query child did not reach its commit boundary")??;
    anyhow::ensure!(
        output.status.code() == Some(86),
        "durability child failed: {output:?}"
    );

    let factory = factory()?;
    let resource = ResourceId::try_new("indexes")?;
    let mut specification = factory.specification(
        component_id()?,
        json!({"query":QUERY, "stream":format!("{COMPONENT}/out")}),
    )?;
    specification
        .dependencies
        .insert("processing".into(), vec![resource.clone()]);
    let source = ComponentId::try_new("47f9883c-b5bc-5df1-aef0-c070a8ad0948")?;
    let sink = ComponentId::try_new("59819eaa-431d-5329-be28-00ff894a33d3")?;
    let query = component_id()?;
    let output_port = PortId::try_new("out")?;
    let input_port = PortId::try_new("in")?;
    let received = Arc::new(Mutex::new(Vec::new()));
    let notification = Arc::new(Notify::new());
    let mut graph = ComputationGraph::builder(GRAPH)
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "processing".into(),
        })?
        .provide_resource(
            resource,
            ResourceHandle::new(
                ResourceRole::StateStore,
                Arc::new(computation::NativeProcessingResource::new(provider(
                    directory.path(),
                )?)),
            ),
        )?
        .source(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-source-1")
                .expect("fixture component name"),
            Box::new(Source {
                descriptor: ComponentDescriptor::try_new(
                    source.clone(),
                    vec![port("out", PortDirection::Output)?],
                )?,
                envelope: None,
            }),
        )
        .component(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-component-1")
                .expect("fixture component name"),
            specification,
            factory,
        )
        .sink(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-sink-1")
                .expect("fixture component name"),
            Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    sink.clone(),
                    vec![port("in", PortDirection::Input)?],
                )?,
                received: received.clone(),
                notification,
            }),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(source.clone(), output_port.clone()),
                Endpoint::new(query.clone(), input_port.clone()),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(query.clone(), output_port.clone()),
                Endpoint::new(sink, input_port),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(
            Endpoint::new(source, output_port.clone()),
            StreamId::try_new("source/out")?,
        )
        .bind_stream(
            Endpoint::new(query, output_port),
            StreamId::try_new(format!("{COMPONENT}/out"))?,
        )
        .build()?;
    let result = tokio::time::timeout(Duration::from_secs(10), graph.start()?).await;
    let cleanup = graph.shutdown().await;
    result??;
    cleanup?;
    let delivered = {
        let received = received
            .lock()
            .map_err(|_| anyhow::anyhow!("durable replay capture poisoned"))?;
        anyhow::ensure!(
            received.len() == 1,
            "graph must replay exactly one output without new input"
        );
        vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: received[0].clone(),
        }]
    };
    aggregate(&delivered, Some(0.0), 10.0, 1)?;
    assert_eq!(delivered[0].envelope.system().sequence(), 2);

    let mut query = create(directory.path(), QUERY, 2)?;
    query.initialize().await?;
    query.start().await?;
    assert!(
        query.continue_transform().await?.is_empty(),
        "graph delivery confirmation was not persisted"
    );
    let committed = query
        .committed_input_progress()?
        .context("process-recovered progress")?
        .wait_ready()
        .await?;
    assert!(committed.persistent);
    assert_eq!(
        committed.checkpoints[&SourceProgressKey::Stream(StreamId::try_new("source/out")?)]
            .sequence,
        1
    );
    assert!(query
        .transform(input(
            SourceChange::Insert {
                element: item(10, 1)
            },
            1
        )?)
        .await?
        .is_empty());
    let updated = query
        .transform(input(
            SourceChange::Replace {
                element: item(12, 2),
            },
            2,
        )?)
        .await?;
    aggregate(&updated, Some(10.0), 12.0, 2)?;
    query.delivery_completed(&updated).await?;
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn process_exit_replays_committed_output_through_graph_current_thread() -> Result<()> {
    process_exit_and_graph_replay().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_exit_replays_committed_output_through_graph_multi_thread() -> Result<()> {
    process_exit_and_graph_replay().await
}
