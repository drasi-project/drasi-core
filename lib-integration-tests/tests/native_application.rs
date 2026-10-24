// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_bootstrap_application::ApplicationBootstrapProvider;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;
use drasi_reaction_application::NativeApplicationReaction;
use drasi_source_application::{NativeApplicationSource, PropertyMapBuilder};
use serde_json::{json, Value};
use std::{num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::sync::{mpsc, RwLock};

async fn receive_name(
    receiver: &mut mpsc::Receiver<InputEnvelope>,
    name: &str,
    deleted: bool,
) -> Result<ChangeEnvelope> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let input = receiver
                .recv()
                .await
                .context("native application receiver closed")?;
            for operation in input.envelope.changes().operations() {
                let record = match (operation, deleted) {
                    (ChangeOperation::Added { after, .. }, false)
                    | (ChangeOperation::Updated { after, .. }, false) => Some(after),
                    (
                        ChangeOperation::Deleted {
                            before: Some(before),
                            ..
                        },
                        true,
                    ) => Some(before),
                    _ => None,
                };
                if let Some(record) = record {
                    let row = QueryChangeCodec::decode_row(record)?;
                    if QueryChangeCodec::row_values_to_json(&row.values)["name"] == name {
                        return Ok(input.envelope);
                    }
                }
            }
        }
    })
    .await
    .with_context(|| format!("native application did not receive {name}"))?
}

async fn scenario() -> Result<()> {
    let source_id = ComponentId::try_new("input")?;
    let query_id = ComponentId::try_new("query")?;
    let reaction_id = ComponentId::try_new("application")?;
    let source_stream = StreamId::try_new("input/out")?;
    let query_stream = StreamId::try_new("query/out")?;
    let (source, input) = NativeApplicationSource::channel(
        "application-test",
        "native-application-trio",
        source_id.as_str(),
        source_stream.clone(),
        NonZeroUsize::new(4).context("capacity")?,
    )?;
    let output = source.output();
    let snapshot = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id.as_str(), "alice"),
                labels: Arc::from(vec![Arc::from("Person")]),
                effective_from: 1,
            },
            properties: PropertyMapBuilder::new()
                .with_string("name", "Alice")
                .build(),
        },
    };
    let bootstrap = Arc::new(ApplicationBootstrapProvider::with_shared_data(Arc::new(
        RwLock::new(vec![snapshot]),
    )));
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "native-application-trio".into(),
            id: query_id.clone(),
            query: "MATCH (n:Person) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: query_stream.clone(),
            outbox_capacity: NonZeroUsize::new(16).context("outbox capacity")?,
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await?
    .with_bootstrap(bootstrap);
    let results = query.results();
    let (reaction, mut received) = NativeApplicationReaction::channel(
        reaction_id.as_str(),
        QueryChangeCodec::schema().descriptor().clone(),
        NonZeroUsize::new(16).context("application capacity")?,
    )?;
    let reaction_input = reaction.input();
    let query_output = Endpoint::new(query_id.clone(), PortId::try_new("out")?);
    let mut graph = ComputationGraph::builder("native-application-trio")
        .source(Box::new(source))
        .transformer(Box::new(query))
        .sink(Box::new(reaction))
        .bind_stream(output.clone(), source_stream)
        .bind_stream(query_output.clone(), query_stream)
        .connect(
            EdgeDefinition::new(output, Endpoint::new(query_id, PortId::try_new("in")?)),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .connect(
            EdgeDefinition::new(query_output, reaction_input),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .build()?;
    input
        .send_node_insert(
            "bob",
            vec!["Person"],
            PropertyMapBuilder::new().with_string("name", "Bob").build(),
        )
        .await?;
    let current_rows = || -> Result<Vec<Value>> {
        let mut rows = results
            .snapshot()?
            .rows
            .values()
            .map(|record| {
                Ok(QueryChangeCodec::row_values_to_json(
                    &QueryChangeCodec::decode_row(record)?.values,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        rows.sort_by_key(Value::to_string);
        Ok(rows)
    };
    let run = graph.start()?;
    let control = run.control();
    let exercise = async {
        let result = async {
            control.startup_report().await?;
            assert_eq!(control.observed().components.len(), 3);
            let live = receive_name(&mut received, "Bob", false).await?;
            assert!(live.system().sequence() > 0);
            assert_eq!(
                current_rows()?,
                [json!({"name":"Alice"}), json!({"name":"Bob"})]
            );

            let source = control.component_handle(&source_id)?;
            source.stop().await?;
            input
                .send_node_update(
                    "alice",
                    vec!["Person"],
                    PropertyMapBuilder::new()
                        .with_string("name", "Alicia")
                        .build(),
                )
                .await?;
            assert_eq!(
                current_rows()?,
                [json!({"name":"Alice"}), json!({"name":"Bob"})]
            );
            source.start().await?;
            receive_name(&mut received, "Alicia", false).await?;
            assert_eq!(
                current_rows()?,
                [json!({"name":"Alicia"}), json!({"name":"Bob"})]
            );

            let reaction = control.component_handle(&reaction_id)?;
            reaction.stop().await?;
            input.send_delete("bob", vec!["Person"]).await?;
            reaction.start().await?;
            receive_name(&mut received, "Bob", true).await?;
            assert_eq!(current_rows()?, [json!({"name":"Alicia"})]);
            Ok::<(), anyhow::Error>(())
        }
        .await;
        control.cancel();
        result
    };
    let (result, exercise) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(run, exercise)
    })
    .await?;
    exercise?;
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
    graph.shutdown().await?;
    Ok(())
}

#[test]
fn native_application_snapshot_and_live_changes_share_one_graph() -> Result<()> {
    for current_thread in [true, false] {
        let mut builder = if current_thread {
            tokio::runtime::Builder::new_current_thread()
        } else {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        };
        builder.enable_all().build()?.block_on(scenario())?;
    }
    Ok(())
}
