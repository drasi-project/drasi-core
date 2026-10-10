// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Executable companion to docs/developer-guide/README.md.
//! Run with `cargo run -p drasi-lib --example stockroom -- basic`
//! or `-- graph`. Both modes check the same inventory changes.

#![allow(clippy::print_stdout)]

use anyhow::{ensure, Context};
use async_trait::async_trait;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementReference, SourceChange},
};
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    computation::v1::*,
    DrasiLib, Query,
};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::{num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::sync::mpsc;

// guide:result-view
#[derive(Default)]
struct ResultView(std::collections::BTreeMap<u64, serde_json::Value>);

impl ResultView {
    fn apply(&mut self, result: &QueryResult) {
        for diff in &result.results {
            match diff {
                ResultDiff::Add {
                    row_signature,
                    data,
                } => {
                    self.0.insert(*row_signature, data.clone());
                }
                ResultDiff::Update {
                    row_signature,
                    after,
                    ..
                }
                | ResultDiff::Aggregation {
                    row_signature,
                    after,
                    ..
                } => {
                    self.0.insert(*row_signature, after.clone());
                }
                ResultDiff::Delete { row_signature, .. } => {
                    self.0.remove(row_signature);
                }
                ResultDiff::Noop => {}
            }
        }
    }
}
// guide:end

// guide:query
const LOW_STOCK: &str = "
MATCH (p:Product)
WHERE p.quantity < p.minimum
RETURN p.sku AS sku, p.quantity AS quantity";
// guide:end

fn properties(quantity: i64) -> drasi_core::models::ElementPropertyMap {
    PropertyMapBuilder::new()
        .with_string("sku", "bolts")
        .with_integer("quantity", quantity)
        .with_integer("minimum", 5)
        .build()
}

async fn next_result(receiver: &mut mpsc::Receiver<QueryResult>) -> anyhow::Result<QueryResult> {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .context("query produced no result within five seconds")?
        .context("result stream closed")
}

fn check_rows(actual: &[serde_json::Value], quantity: Option<i64>) -> anyhow::Result<()> {
    let expected = quantity
        .map(|quantity| serde_json::json!({"sku": "bolts", "quantity": quantity}))
        .into_iter()
        .collect::<Vec<_>>();
    ensure!(actual == expected, "expected {expected:?}, got {actual:?}");
    Ok(())
}

async fn finish(core: &DrasiLib, work: anyhow::Result<()>) -> anyhow::Result<()> {
    let cleanup = core.shutdown().await;
    if let Err(error) = &cleanup {
        eprintln!("Stockroom shutdown failed: {error:#}");
    }
    work?;
    cleanup?;
    Ok(())
}

async fn basic() -> anyhow::Result<()> {
    // guide:basic-build
    let (source, input) = ApplicationSource::new(
        "inventory",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let (reaction, output) = ApplicationReaction::new("alerts", vec!["low-stock".into()]);
    let mut receiver = output
        .take_receiver()
        .await
        .context("alerts receiver was already taken")?;
    let core = DrasiLib::builder()
        .with_id("stockroom")
        .with_source(source)
        .with_query(
            Query::cypher("low-stock")
                .query(LOW_STOCK)
                .from_source("inventory")
                .enable_bootstrap(false)
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await?;
    // guide:end

    let work = async {
        let mut view = ResultView::default();
        core.start().await?;
        tokio::time::timeout(
            Duration::from_secs(5),
            core.computation_component("alerts")?.wait_started(),
        )
        .await??;
        // guide:basic-input
        input
            .send_node_insert("bolts", vec!["Product"], properties(3))
            .await?;
        let added = next_result(&mut receiver).await?;
        view.apply(&added);
        check_rows(&view.0.values().cloned().collect::<Vec<_>>(), Some(3))?;
        println!("basic: added {:?}", added.results);
        check_rows(&core.get_query_results("low-stock").await?, Some(3))?;

        input
            .send_node_update("bolts", vec!["Product"], properties(2))
            .await?;
        let updated = next_result(&mut receiver).await?;
        view.apply(&updated);
        check_rows(&view.0.values().cloned().collect::<Vec<_>>(), Some(2))?;
        println!("basic: updated {:?}", updated.results);
        check_rows(&core.get_query_results("low-stock").await?, Some(2))?;

        input.send_delete("bolts", vec!["Product"]).await?;
        let deleted = next_result(&mut receiver).await?;
        view.apply(&deleted);
        check_rows(&view.0.values().cloned().collect::<Vec<_>>(), None)?;
        println!("basic: deleted {:?}", deleted.results);
        check_rows(&core.get_query_results("low-stock").await?, None)?;
        // guide:end
        Ok(())
    }
    .await;
    finish(&core, work).await
}

fn endpoint(component: &str, port: &str) -> anyhow::Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(component)?,
        PortId::try_new(port)?,
    ))
}

fn descriptor(
    id: &str,
    ports: &[(&str, PortDirection, SchemaDescriptor)],
) -> anyhow::Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        ComponentId::try_new(id)?,
        ports
            .iter()
            .map(|(name, direction, schema)| {
                Ok(PortDescriptor::new(
                    PortId::try_new(*name)?,
                    *direction,
                    schema.clone(),
                    PipeRequirements::default(),
                ))
            })
            .collect::<anyhow::Result<_>>()?,
    )?)
}

struct Inventory {
    descriptor: ComponentDescriptor,
    receiver: mpsc::Receiver<SourceChange>,
    sequence: u64,
}

#[async_trait]
impl ComputationComponent for Inventory {
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

// guide:native-source
#[async_trait]
impl EnvelopeSource for Inventory {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let Some(change) = self.receiver.recv().await else {
            return Ok(None);
        };
        self.sequence = self.sequence.checked_add(1).context("sequence exhausted")?;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: GraphChangeCodec::encode_change(
                change,
                StreamId::try_new("inventory/out")?,
                self.sequence,
                None,
            )?,
        }))
    }
}
// guide:end

struct Audit {
    descriptor: ComponentDescriptor,
    sequence: u64,
}

#[async_trait]
impl ComputationComponent for Audit {
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

// guide:transform
#[async_trait]
impl Transformer for Audit {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        self.sequence = self.sequence.checked_add(1).context("sequence exhausted")?;
        let mut envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &changes,
            StreamId::try_new("audit/out")?,
            self.sequence,
        )?;
        envelope.append_annotation(ContextEntry::try_new(
            self.descriptor.id().clone(),
            "checked-by",
            ContextValue::String("stockroom".into()),
        )?)?;
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }])
    }
}
// guide:end

struct Alerts {
    descriptor: ComponentDescriptor,
    sender: mpsc::Sender<QueryResult>,
}

#[async_trait]
impl ComputationComponent for Alerts {
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

// guide:sink
#[async_trait]
impl EnvelopeSink for Alerts {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.sender
            .send(QueryChangeCodec::to_legacy_result(&input.envelope)?)
            .await
            .context("application stopped receiving alerts")
    }
}
// guide:end

async fn graph() -> anyhow::Result<()> {
    let (input, changes) = mpsc::channel(8);
    let (sender, mut receiver) = mpsc::channel(8);
    let graph_schema = GraphChangeCodec::schema().descriptor().clone();
    let inventory = Inventory {
        descriptor: descriptor(
            "inventory",
            &[("out", PortDirection::Output, graph_schema.clone())],
        )?,
        receiver: changes,
        sequence: 0,
    };
    let audit = Audit {
        descriptor: descriptor(
            "audit",
            &[
                ("in", PortDirection::Input, graph_schema.clone()),
                ("out", PortDirection::Output, graph_schema),
            ],
        )?,
        sequence: 0,
    };
    let alerts = Alerts {
        descriptor: descriptor(
            "alerts",
            &[(
                "in",
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
            )],
        )?,
        sender,
    };
    // guide:native-query
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "__drasi_lib_runtime__".into(),
            id: ComponentId::try_new("low-stock")?,
            query: LOW_STOCK.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("low-stock/out")?,
            outbox_capacity: NonZeroUsize::new(128).expect("128 is nonzero"),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await?;
    // guide:end
    // guide:batch
    let mut batch = ComponentBatch::builder()
        .source(Box::new(inventory))
        .transformer(Box::new(audit))
        .query(Box::new(query))
        .sink(Box::new(alerts));
    for producer in ["inventory", "audit", "low-stock"] {
        batch = batch.bind_stream(
            endpoint(producer, "out")?,
            StreamId::try_new(format!("{producer}/out"))?,
        );
    }
    for (from, to) in [("inventory", "audit"), ("audit", "low-stock"), ("low-stock", "alerts")] {
        batch = batch.connect(
            EdgeDefinition::new(endpoint(from, "out")?, endpoint(to, "in")?),
            Box::new(BoundedPipeConfig { capacity: 8 }),
        );
    }
    let core = DrasiLib::builder()
        .with_id("stockroom")
        .with_components(batch.build()?)
        .build()
        .await?;
    // guide:end
    let work = async {
        core.start().await?;
        let metadata = ElementMetadata {
            reference: ElementReference {
                source_id: "inventory".into(),
                element_id: "bolts".into(),
            },
            labels: vec!["Product".into()].into(),
            effective_from: 1,
        };
        for (change, quantity, label) in [
            (
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: metadata.clone(),
                        properties: properties(3),
                    },
                },
                Some(3),
                "added",
            ),
            (
                SourceChange::Update {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            effective_from: 2,
                            ..metadata.clone()
                        },
                        properties: properties(2),
                    },
                },
                Some(2),
                "updated",
            ),
            (
                SourceChange::Delete {
                    metadata: ElementMetadata {
                        effective_from: 3,
                        ..metadata
                    },
                },
                None,
                "deleted",
            ),
        ] {
            input.send(change).await?;
            let result = next_result(&mut receiver).await?;
            println!("graph: {label} {:?}", result.results);
            check_rows(&core.get_query_results("low-stock").await?, quantity)?;
        }
        Ok(())
    }
    .await;
    finish(&core, work).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        None | Some("basic") => basic().await,
        Some("graph") => graph().await,
        Some(value) => anyhow::bail!("unknown mode {value:?}; use basic or graph"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn result_view_keeps_distinct_identities_and_handles_aggregation() {
        use drasi_lib::channels::{QueryResult, ResultDiff};
        let mut view = super::ResultView::default();
        let mut result = QueryResult::new(
            "q".into(),
            1,
            std::time::SystemTime::now().into(),
            vec![
                ResultDiff::Add {
                    row_signature: 1,
                    data: serde_json::json!({"total": 3}),
                },
                ResultDiff::Add {
                    row_signature: 2,
                    data: serde_json::json!({"total": 3}),
                },
            ],
            Default::default(),
        );
        view.apply(&result);
        assert_eq!(view.0.len(), 2);
        result.results = vec![
            ResultDiff::Aggregation {
                row_signature: 1,
                before: Some(serde_json::json!({"total": 3})),
                after: serde_json::json!({"total": 2}),
            },
            ResultDiff::Delete {
                row_signature: 2,
                data: serde_json::json!({"total": 3}),
            },
            ResultDiff::Noop,
        ];
        view.apply(&result);
        assert_eq!(view.0.len(), 1);
        assert_eq!(view.0[&1], serde_json::json!({"total": 2}));
    }

    #[tokio::test]
    async fn ordinary_stockroom() -> anyhow::Result<()> {
        super::basic().await
    }

    #[tokio::test]
    async fn direct_graph_stockroom() -> anyhow::Result<()> {
        super::graph().await
    }
}
