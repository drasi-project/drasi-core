// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::num::NonZeroUsize;

const TEXT: &str = "MATCH (n:Selected) RETURN n.value AS value";

fn settings(component: &str, from: &str) -> serde_json::Value {
    json!({
        "inputs": [{"component": component, "pipeline": ["labels"]}],
        "middleware": [{
            "name": "labels", "kind": "relabel",
            "config": {"labelMappings": {from: "Selected"}}
        }]
    })
}

fn configured(component: &str, from: &str) -> serde_json::Value {
    let mut configuration = settings(component, from);
    configuration["query"] = json!(TEXT);
    configuration["outbox_capacity"] = json!(8);
    configuration
}

fn binding(component: ComponentId, stream: &str) -> Result<InputBinding> {
    Ok(InputBinding {
        input_port: PortId::try_new("in")?,
        output: OutputEndpoint::new(component, PortId::try_new("out")?),
        stream: StreamId::try_new(stream)?,
    })
}

fn foreign_item(id: &str, value: i64, time: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("not-the-input-component", id),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(json!({"value": value})),
    }
}

async fn parity_and_aliases() -> Result<()> {
    for alias in [false, true] {
        let directory = tempfile::tempdir()?;
        let configuration = configured("source", "Item");
        let execution: QueryExecutionSettings = serde_json::from_value(settings("source", "Item"))?;
        let mut native = factory()?.create_component_with_processing(
            ComponentId::try_new(COMPONENT)?,
            configuration,
            scope(1),
            provider(&directory.path().join("native"))?,
        )?;
        let mut ordinary_config = drasi_lib::Query::cypher(COMPONENT)
            .query(TEXT)
            .from_input("source")
            .enable_bootstrap(false)
            .with_outbox_capacity(8)
            .build();
        ordinary_config.inputs = execution.inputs.clone();
        ordinary_config.middleware = execution.middleware.clone();
        let mut linked = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: GRAPH.into(),
                id: ComponentId::try_new(COMPONENT)?,
                query: TEXT.into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new(format!("{COMPONENT}/out"))?,
                outbox_capacity: NonZeroUsize::new(8).expect("positive retention"),
            },
            provider(&directory.path().join("linked"))?,
            QueryOptions::default(),
            execution,
            Some(drasi_computation_transformer_query::default_middleware_registry()),
        )
        .await?
        .with_query_configuration(ordinary_config)?;
        let source = if alias {
            drasi_computation_contracts::query_source_component_id(COMPONENT, "source")?
        } else {
            ComponentId::try_new("source")?
        };
        let bindings = [
            binding(source, "source/out")?,
            binding(ComponentId::try_new("other")?, "other/out")?,
        ];
        native.bind_inputs(&bindings).await?;
        linked.bind_inputs(&bindings).await?;
        native.start().await?;
        linked.start().await?;
        let ignored = InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_change(
                SourceChange::Insert {
                    element: foreign_item("ignored", 99, 1),
                },
                StreamId::try_new("other/out")?,
                1,
                None,
            )?,
        };
        assert!(native.transform(ignored.clone()).await?.is_empty());
        assert!(linked.transform(ignored).await?.is_empty());
        for (index, change) in [
            SourceChange::Insert {
                element: foreign_item("one", 1, 1),
            },
            SourceChange::Replace {
                element: foreign_item("one", 2, 2),
            },
            SourceChange::Delete {
                metadata: foreign_item("one", 2, 3).get_metadata().clone(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let input = input(change, index as u64 + 1)?;
            let actual = native.transform(input.clone()).await?;
            let expected = linked.transform(input).await?;
            assert_eq!(actual.len(), 1);
            assert_eq!(actual.len(), expected.len());
            let actual_rows = QueryChangeCodec::decode_evaluation(&actual[0].envelope)?;
            let expected_rows = QueryChangeCodec::decode_evaluation(&expected[0].envelope)?;
            assert_eq!(actual_rows.len(), expected_rows.len());
            assert!(actual_rows
                .iter()
                .zip(&expected_rows)
                .all(|(left, right)| left.data_eq(right)));
            native.delivery_completed(&actual).await?;
            linked.delivery_completed(&expected).await?;
        }
        native.stop().await?;
        linked.stop().await?;
    }
    Ok(())
}

async fn derived_input_scope() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut upstream = factory()?.create_component_with_processing(
        ComponentId::try_new("upstream")?,
        json!({"query":"MATCH (n:Item) RETURN n.value AS value", "result":{"label":"UpstreamRow"}}),
        scope(1),
        provider(&directory.path().join("upstream"))?,
    )?;
    let mut downstream = factory()?.create_component_with_processing(
        ComponentId::try_new(COMPONENT)?,
        configured("upstream", "UpstreamRow"),
        scope(1),
        provider(&directory.path().join("downstream"))?,
    )?;
    downstream
        .bind_inputs(&[binding(ComponentId::try_new("upstream")?, "upstream/out")?])
        .await?;
    upstream.start().await?;
    downstream.start().await?;
    for sequence in 1..=2 {
        let output = upstream
            .transform(input(
                SourceChange::Replace {
                    element: item(sequence as i64, sequence),
                },
                sequence,
            )?)
            .await?;
        upstream.delivery_completed(&output).await?;
        let [output] = output.as_slice() else {
            anyhow::bail!("missing upstream output");
        };
        let converted = downstream
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: output.envelope.clone(),
            })
            .await?;
        assert_eq!(
            converted.len(),
            1,
            "middleware must use the declared input, not the result dataset namespace"
        );
        let rows = QueryChangeCodec::decode_evaluation(&converted[0].envelope)?;
        let value = match &rows[0] {
            QueryPartEvaluationContext::Adding { after, .. }
            | QueryPartEvaluationContext::Updating { after, .. } => after,
            row => anyhow::bail!("unexpected downstream result: {row:?}"),
        };
        assert_eq!(
            value.get("value"),
            Some(&VariableValue::from(json!(sequence)))
        );
        downstream.delivery_completed(&converted).await?;
    }
    downstream.stop().await?;
    upstream.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_middleware_matches_linked_scope_and_aliases_current_thread() -> Result<()> {
    parity_and_aliases().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_query_middleware_matches_linked_scope_and_aliases_multi_thread() -> Result<()> {
    parity_and_aliases().await
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_middleware_uses_actual_query_producer_scope() -> Result<()> {
    derived_input_scope().await
}

#[test]
fn shared_query_middleware_rejects_unknown_factory_before_opening_indexes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut configuration = configured("source", "Item");
    configuration["middleware"][0]["kind"] = json!("missing-middleware");
    let error = factory()?
        .create_component_with_processing(
            ComponentId::try_new(COMPONENT)?,
            configuration,
            scope(1),
            provider(directory.path())?,
        )
        .err()
        .context("unknown middleware was accepted")?;
    assert!(
        format!("{error:#}").contains("missing-middleware"),
        "{error:#}"
    );
    assert!(std::fs::read_dir(directory.path())?.next().is_none());
    Ok(())
}

struct ScopedBootstrap;

#[async_trait]
impl ComputationBootstrapProvider for ScopedBootstrap {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: foreign_item("one", 1, 1),
            },
            StreamId::try_new("snapshot-source")?,
            1,
            None,
        )?;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures_util::stream::iter([Ok(envelope)])),
            watermarks: Vec::new(),
        })
    }

    fn input_origin(&self, stream: &StreamId) -> Result<Option<OutputEndpoint>> {
        anyhow::ensure!(
            stream.as_str() == "snapshot-source",
            "unexpected snapshot stream"
        );
        Ok(Some(OutputEndpoint::new(
            ComponentId::try_new("source")?,
            PortId::try_new("out")?,
        )))
    }
}

async fn bootstrap_origin_and_live_binding() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/provider-query"
        })
        .context("missing provider query factory")?;
    let mut configuration = configured("source", "Item");
    configuration["volatile"] = json!(true);
    let mut query = factory.create_component_with_bootstrap(
        ComponentId::try_new(COMPONENT)?,
        configuration,
        scope(1),
        Arc::new(computation::NativeBootstrapResource(Arc::new(
            ScopedBootstrap,
        ))),
    )?;
    query
        .bind_inputs(&[binding(ComponentId::try_new("source")?, "source/out")?])
        .await?;
    query.start().await?;
    let readers = query.output_readers();
    let snapshot = readers
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|port| port.snapshot())
        .context("missing native snapshot")?
        .snapshot()
        .await?;
    assert_eq!(
        snapshot.records().len(),
        1,
        "bootstrap must select middleware from the provider's actual origin"
    );
    let row = QueryChangeCodec::decode_row(
        snapshot
            .records()
            .values()
            .next()
            .context("missing bootstrapped row")?,
    )?;
    assert_eq!(
        row.values.get("value"),
        Some(&VariableValue::from(json!(1)))
    );
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: foreign_item("one", 2, 2),
            },
            1,
        )?)
        .await?;
    let [output] = output.as_slice() else {
        anyhow::bail!("live input binding was not restored");
    };
    assert!(matches!(
        QueryChangeCodec::decode_evaluation(&output.envelope)?.as_slice(),
        [QueryPartEvaluationContext::Updating { after, .. }]
            if after.get("value") == Some(&VariableValue::from(json!(2)))
    ));
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_middleware_bootstrap_origin_current_thread() -> Result<()> {
    bootstrap_origin_and_live_binding().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_query_middleware_bootstrap_origin_multi_thread() -> Result<()> {
    bootstrap_origin_and_live_binding().await
}

async fn custom_factory_and_actual_index(persistent: bool) -> Result<()> {
    let path = library()
        .parent()
        .context("missing target directory")?
        .join("examples")
        .join(format!(
            "{}native_custom_query{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX,
        ));
    let plugin = computation::load(path)?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == if persistent {
                    "drasi.experimental/indexed-query"
                } else {
                    "drasi.experimental/volatile-query"
                }
        })
        .context("missing custom query factory")?;
    let directory = tempfile::tempdir()?;
    let mut configuration = json!({
        "query":"MATCH (n:Item) RETURN n.visits AS visits, n.factory AS factory",
        "inputs":[{"component":"source","pipeline":["custom"]}],
        "middleware":[{"name":"custom","kind":"query-custom","config":{}}]
    });
    let mut query = if persistent {
        factory.create_component_with_processing(
            ComponentId::try_new(COMPONENT)?,
            configuration,
            scope(1),
            provider(directory.path())?,
        )?
    } else {
        configuration["volatile"] = json!(true);
        factory.create_component(ComponentId::try_new(COMPONENT)?, configuration)?
    };
    query
        .bind_inputs(&[binding(ComponentId::try_new("source")?, "source/out")?])
        .await?;
    query.start().await?;
    let mut instance = None;
    for sequence in 1..=2 {
        let output = query
            .transform(input(
                SourceChange::Replace {
                    element: foreign_item("one", 7, sequence),
                },
                sequence,
            )?)
            .await?;
        assert_eq!(output.len(), 1);
        let rows = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
        let after = match &rows[0] {
            QueryPartEvaluationContext::Adding { after, .. }
            | QueryPartEvaluationContext::Updating { after, .. } => after,
            row => anyhow::bail!("unexpected custom middleware output: {row:?}"),
        };
        assert_eq!(
            after.get("visits"),
            Some(&VariableValue::from(json!(sequence))),
            "custom middleware did not read the actual previous element"
        );
        let factory = after
            .get("factory")
            .context("missing middleware instance")?
            .clone();
        if let Some(previous) = &instance {
            assert_eq!(
                previous, &factory,
                "middleware must not be reconstructed per input"
            );
        }
        instance = Some(factory);
        query.delivery_completed(&output).await?;
        if !persistent && sequence == 1 {
            let readers = query.output_readers();
            let reader = readers
                .ports()
                .get(&PortId::try_new("out")?)
                .and_then(|port| port.query())
                .context("missing query reader")?;
            let before = reader.query_head().await?;
            query.stop().await?;
            assert!(reader.query_head().await.is_err());
            query.start().await?;
            assert_eq!(
                reader.query_head().await?,
                before,
                "healthy volatile restart changed its output epoch"
            );
        }
    }
    if !persistent {
        let mut bad = foreign_item("one", 7, 3);
        let Element::Node { properties, .. } = &mut bad else {
            unreachable!()
        };
        properties.insert("fail", drasi_core::models::ElementValue::Bool(true));
        assert!(query
            .transform(input(SourceChange::Replace { element: bad }, 3)?)
            .await
            .is_err());
        query.stop().await?;
        assert!(
            query.start().await.is_err(),
            "failed volatile state must not restart empty"
        );
    }
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_middleware_custom_factory_lifetime_current_thread() -> Result<()> {
    custom_factory_and_actual_index(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_query_middleware_custom_factory_lifetime_multi_thread() -> Result<()> {
    custom_factory_and_actual_index(true).await
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_middleware_volatile_lifetime_and_failed_restart() -> Result<()> {
    custom_factory_and_actual_index(false).await
}
