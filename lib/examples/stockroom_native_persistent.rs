// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Retained native input, query output and transactional consumer completion.
//! Run `-- <new-directory> stage`, then `-- <directory> resume`.
//! Stage deliberately stops before handling an already retained query output.

#![allow(clippy::print_stdout)]

use anyhow::{ensure, Context};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    interface::FailureMode,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{computation::v1::*, DrasiLib};
use std::{num::NonZeroUsize, path::Path, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex};

const GRAPH: &str = "__drasi_lib_runtime__";
const INSTANCE: &str = "stockroom-native-persistent";

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("example capacities are nonzero")
}

fn endpoint(component: &str, port: &str) -> anyhow::Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(component)?,
        PortId::try_new(port)?,
    ))
}

fn descriptor(id: &str, input: bool) -> anyhow::Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        ComponentId::try_new(id)?,
        vec![PortDescriptor::new(
            PortId::try_new(if input { "in" } else { "out" })?,
            if input {
                PortDirection::Input
            } else {
                PortDirection::Output
            },
            if input {
                QueryChangeCodec::schema()
            } else {
                GraphChangeCodec::schema()
            }
            .descriptor()
            .clone(),
            PipeRequirements::default(),
        )],
    )?)
}

fn codec() -> anyhow::Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(size(1024 * 1024));
    codec.register_schema(GraphChangeCodec::schema())?;
    codec.register_schema(QueryChangeCodec::schema())?;
    Ok(codec)
}

fn channel_definition(stream: &str, subscriber: &str) -> anyhow::Result<QosChannelDefinition> {
    Ok(QosChannelDefinition {
        stream: StreamId::try_new(stream)?,
        capacity: size(32),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: [(subscriber.into(), SubscriptionStart::Earliest)].into(),
    })
}

struct Inventory {
    descriptor: ComponentDescriptor,
    admission: Arc<SourceAdmission>,
}

#[async_trait]
impl ComputationComponent for Inventory {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        ComponentRecovery::admitted(self.admission.channel().durability())
            .replay_to_durable_delivery()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Inventory {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        Some(self.admission.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        anyhow::bail!("the graph must drive SourceAdmission, not next")
    }
}

// guide:transactional-handler
struct SaveAlert;

fn alert_key(identity: &RecordId) -> String {
    let key = identity
        .value()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("alert/{}/{key}", identity.namespace())
}

#[async_trait]
impl TransactionalDeliveryHandler for SaveAlert {
    async fn handle(
        &self,
        item: DeliveryItem<'_>,
        state: &TransactionContext<'_>,
    ) -> anyhow::Result<()> {
        match item.operation {
            ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
                let row = QueryChangeCodec::decode_row(after)?;
                let quantity = row
                    .values
                    .get("quantity")
                    .and_then(|value| value.as_i64())
                    .context("query omitted integer quantity")?;
                state
                    .put(
                        &alert_key(after.identity()),
                        ElementValue::Integer(quantity),
                    )
                    .await?;
            }
            ChangeOperation::Deleted { identity, .. } => {
                state.remove(&alert_key(identity.identity())).await?;
            }
        }
        let count = match state.get("handled").await? {
            None => 0,
            Some(ElementValue::Integer(value)) => value,
            value => anyhow::bail!("invalid handled count: {value:?}"),
        };
        state
            .put("handled", ElementValue::Integer(count + 1))
            .await?;
        Ok(())
    }
}
// guide:end

struct Alerts {
    descriptor: ComponentDescriptor,
    runner: Arc<Mutex<DeliveryRunner>>,
    contract: ComponentRecovery,
    stage: bool,
    completed: mpsc::Sender<()>,
}

#[async_trait]
impl ComputationComponent for Alerts {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        self.contract.clone()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.runner.lock().await.shutdown().await
    }
}

#[async_trait]
impl EnvelopeSink for Alerts {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if self.stage {
            self.completed.send(()).await?;
            std::future::pending::<()>().await;
        }
        self.runner
            .lock()
            .await
            .deliver_transactional(&input, &SaveAlert)
            .await?;
        self.completed.send(()).await?;
        Ok(())
    }
}

async fn run(directory: &Path, stage: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(directory)?;
    let provider = LegacyIndexProviderAdapter::scoped(
        Arc::new(
            RocksDbIndexProvider::new(directory, false, false)
                .with_memory_budget_bytes(128 << 20)?,
        ),
        INSTANCE,
    )?;
    // guide:persistent-channels
    let incoming = QosChannel::persistent_with_recovery(
        channel_definition("inventory/out", "low-stock")?,
        provider.create_indexes(GRAPH, "input-journal").await?,
        codec()?,
        "inventory",
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: INSTANCE.into(),
            graph_id: GRAPH.into(),
            component_id: ComponentId::try_new("inventory")?,
            failure_scope: FailureMode::ProcessRestart,
            max_producers: size(1),
            receipts_per_producer: size(32),
        }),
    )
    .await?;
    let outgoing = QosChannel::persistent_with_recovery(
        channel_definition("low-stock/out", "alerts")?,
        provider.create_indexes(GRAPH, "output-journal").await?,
        codec()?,
        "low-stock",
        QosRecoveryOptions::Replay(ReplayOptions {
            failure_scope: FailureMode::ProcessRestart,
            receipt_capacity: size(32),
        }),
    )
    .await?;
    // guide:end
    let admission = SourceAdmission::new(incoming.clone(), PortId::try_new("out")?).await?;
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: ComponentId::try_new("low-stock")?,
            query: "MATCH (p:Product) WHERE p.quantity < p.minimum RETURN p.sku AS sku, p.quantity AS quantity".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("low-stock/out")?,
            outbox_capacity: size(32),
        },
        provider.clone(),
    ).await?.with_recovery_scope(INSTANCE)?;
    let query = TransactionTransformer::from_query(query);
    let indexes = provider.create_indexes(GRAPH, "alert-completion").await?;
    let contract = ComponentRecovery::transactional_consumer(&indexes)?;
    let runner = Arc::new(Mutex::new(DeliveryRunner::new_transactional(
        DeliveryScope::new(
            INSTANCE.into(),
            GRAPH.into(),
            ComponentId::try_new("alerts")?,
        )?,
        indexes,
        codec()?,
        DeliveryOptions {
            scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
            max_streams: size(1),
            receipts_per_stream: size(32),
            retry: DeliveryRetryPolicy::default(),
        },
    )?));
    let (completed, mut completion) = mpsc::channel(4);
    let mut batch = ComponentBatch::builder()
        .source(Box::new(Inventory {
            descriptor: descriptor("inventory", false)?,
            admission: admission.clone(),
        }))
        .query(Box::new(query))
        .sink(Box::new(Alerts {
            descriptor: descriptor("alerts", true)?,
            runner: runner.clone(),
            contract,
            stage,
            completed,
        }))
        .bind_stream(
            endpoint("inventory", "out")?,
            StreamId::try_new("inventory/out")?,
        )
        .bind_stream(
            endpoint("low-stock", "out")?,
            StreamId::try_new("low-stock/out")?,
        );
    for (name, handle) in [
        ("indexes", provider.resource()),
        ("input-journal", incoming.resource()),
        ("output-journal", outgoing.resource()),
    ] {
        let id = ResourceId::try_new(name)?;
        batch = batch
            .declare_resource(ResourceSpecification {
                id: id.clone(),
                role: handle.role(),
                ownership: ResourceOwnership::Graph,
                binding: name.into(),
            })?
            .provide_resource(id, handle)?;
    }
    batch = batch
        .connect(
            EdgeDefinition::new(endpoint("inventory", "out")?, endpoint("low-stock", "in")?),
            Box::new(
                incoming
                    .definition()
                    .pipe(ResourceId::try_new("input-journal")?, "low-stock"),
            ),
        )
        .connect(
            EdgeDefinition::new(endpoint("low-stock", "out")?, endpoint("alerts", "in")?),
            Box::new(
                outgoing
                    .definition()
                    .pipe(ResourceId::try_new("output-journal")?, "alerts"),
            ),
        );
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_components(batch.build()?)
        .build()
        .await?;
    let work = async {
        core.start().await?;
        tokio::time::timeout(
            Duration::from_secs(10),
            core.computation_component("inventory")?.wait_started(),
        )
        .await??;
        if stage {
            ensure!(
                core.get_query_results("low-stock").await?.is_empty(),
                "stage requires a new directory"
            );
            let input = GraphChangeCodec::encode_change(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("inventory", "bolts"),
                            labels: vec!["Product".into()].into(),
                            effective_from: 1,
                        },
                        properties: ElementPropertyMap::from(
                            serde_json::json!({"sku":"bolts","quantity":3,"minimum":5}),
                        ),
                    },
                },
                StreamId::try_new("inventory/out")?,
                1,
                None,
            )?;
            let producer = admission
                .register_producer(ComponentId::try_new("stockroom-app")?)
                .await?;
            let first = admission.admit(&producer, 1, &input).await?;
            let retry = admission.admit(&producer, 1, &input).await?;
            ensure!(
                first == retry,
                "same submission must return the same receipt"
            );
        }
        tokio::time::timeout(Duration::from_secs(10), completion.recv())
            .await?
            .context("alert output closed")?;
        ensure!(
            core.get_query_results("low-stock").await?
                == vec![serde_json::json!({"sku":"bolts","quantity":3})]
        );
        if stage {
            ensure!(
                runner.lock().await.progress().await?.is_empty(),
                "stage must not handle the output"
            );
            println!("retained one alert without handling it");
        } else {
            let progress = runner.lock().await.progress().await?;
            ensure!(
                progress.len() == 1 && progress[0].completed_operations == 1,
                "expected one completed alert: {progress:?}"
            );
            println!("restored and handled the retained alert without resubmitting inventory");
        }
        Ok(())
    }
    .await;
    let cleanup = core.shutdown().await;
    if let Err(error) = &cleanup {
        eprintln!("native Stockroom cleanup failed: {error:#}");
    }
    work?;
    cleanup?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let [directory, mode] = args.as_slice() else {
        anyhow::bail!("usage: stockroom_native_persistent <directory> <stage|resume>");
    };
    let stage = match mode.as_str() {
        "stage" => true,
        "resume" => false,
        _ => anyhow::bail!("mode must be stage or resume"),
    };
    run(Path::new(directory), stage).await
}

#[cfg(test)]
mod tests {
    #[tokio::test(flavor = "current_thread")]
    async fn retained_output_survives_reconstruction() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        super::run(directory.path(), true).await?;
        super::run(directory.path(), false).await
    }
}
