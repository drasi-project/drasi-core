// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{QueryJoinConfig, QueryJoinKeyConfig};
use drasi_core::computation::InMemoryComputationProvider;
use drasi_host_sdk::computation::NativeFactory;

const JOIN_QUERY: &str = "MATCH (o:orders)-[:PLACED_BY]->(c:customers) RETURN o.id AS order_id, o.amount AS amount, c.name AS customer";

fn join_settings() -> Vec<QueryJoinConfig> {
    [
        ("PLACED_BY", "customer_id", "id"),
        ("UNUSED", "unused", "unused"),
    ]
    .into_iter()
    .map(|(id, order_key, customer_key)| QueryJoinConfig {
        id: id.into(),
        keys: vec![
            QueryJoinKeyConfig {
                label: "orders".into(),
                property: order_key.into(),
            },
            QueryJoinKeyConfig {
                label: "customers".into(),
                property: customer_key.into(),
            },
        ],
    })
    .collect()
}

fn plugin_factory(implementation: &str) -> Result<Arc<NativeFactory>> {
    computation::load(library())?
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == implementation)
        .cloned()
        .context("missing query factory")
}

fn node(label: &str, id: &str, properties: serde_json::Value, time: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(label, id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn expected(values: &[(i64, i64, &str)]) -> Vec<serde_json::Value> {
    values
        .iter()
        .map(|(order, amount, customer)| {
            json!({"order_id":order, "amount":amount, "customer":customer})
        })
        .collect()
}

fn rows(snapshot: &OutputSnapshot) -> Result<Vec<serde_json::Value>> {
    let mut rows = snapshot
        .records()
        .values()
        .map(|record| {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(record)?.values,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by_key(|row| row["order_id"].as_i64().expect("query returns an order ID"));
    Ok(rows)
}

async fn component_rows(component: &dyn ComputationComponent) -> Result<Vec<serde_json::Value>> {
    let readers = component.output_readers();
    let snapshot = readers
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|readers| readers.snapshot())
        .context("missing result snapshot")?
        .snapshot()
        .await?;
    rows(&snapshot)
}

async fn matches_builtin() -> Result<()> {
    for (language, core_language) in [
        ("cypher", ComputationQueryLanguage::Cypher),
        ("gql", ComputationQueryLanguage::Gql),
    ] {
        let joins = join_settings();
        let mut query = plugin_factory("drasi.experimental/volatile-query")?.create_component(
            ComponentId::try_new("62180e0c-6690-5dff-9c21-c145e9a23680")?,
            json!({
                "query": JOIN_QUERY, "language": language,
                "volatile": true, "joins": joins,
            }),
        )?;
        query.initialize().await?;
        let mut reference = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: "synthetic-join-reference".into(),
                id: ComponentId::try_new("60ac25b9-0f08-58ce-8525-aa526d46f2d6")?,
                query: JOIN_QUERY.into(),
                language: core_language,
                output_stream: StreamId::try_new("reference/out")?,
                outbox_capacity: NonZeroUsize::new(16).expect("positive capacity"),
            },
            Arc::new(InMemoryComputationProvider),
            QueryOptions::default(),
            QueryExecutionSettings {
                joins,
                ..QueryExecutionSettings::default()
            },
            None,
        )
        .await?;
        query.start().await?;
        reference.start().await?;
        for (index, (change, expected)) in [
            (
                SourceChange::Insert {
                    element: node(
                        "orders",
                        "10",
                        json!({"id":10,"amount":100,"customer_id":1}),
                        1,
                    ),
                },
                expected(&[]),
            ),
            (
                SourceChange::Insert {
                    element: node("customers", "1", json!({"id":1,"name":"Ada"}), 2),
                },
                expected(&[(10, 100, "Ada")]),
            ),
            (
                SourceChange::Insert {
                    element: node(
                        "orders",
                        "20",
                        json!({"id":20,"amount":200,"customer_id":2}),
                        3,
                    ),
                },
                expected(&[(10, 100, "Ada")]),
            ),
            (
                SourceChange::Insert {
                    element: node("customers", "2", json!({"id":2,"name":"Bea"}), 4),
                },
                expected(&[(10, 100, "Ada"), (20, 200, "Bea")]),
            ),
            (
                SourceChange::Replace {
                    element: node(
                        "orders",
                        "10",
                        json!({"id":10,"amount":110,"customer_id":2}),
                        5,
                    ),
                },
                expected(&[(10, 110, "Bea"), (20, 200, "Bea")]),
            ),
            (
                SourceChange::Replace {
                    element: node("customers", "2", json!({"id":2,"name":"Bex"}), 6),
                },
                expected(&[(10, 110, "Bex"), (20, 200, "Bex")]),
            ),
            (
                SourceChange::Delete {
                    metadata: node("customers", "2", json!({}), 7).get_metadata().clone(),
                },
                expected(&[]),
            ),
            (
                SourceChange::Insert {
                    element: node("customers", "2", json!({"id":2,"name":"Cam"}), 8),
                },
                expected(&[(10, 110, "Cam"), (20, 200, "Cam")]),
            ),
            (
                SourceChange::Delete {
                    metadata: node("orders", "10", json!({}), 9).get_metadata().clone(),
                },
                expected(&[(20, 200, "Cam")]),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let input = input(change, index as u64 + 1)?;
            let actual = query.transform(input.clone()).await?;
            let reference_output = reference.transform(input).await?;
            assert_eq!(actual.len(), reference_output.len());
            query.delivery_completed(&actual).await?;
            reference.delivery_completed(&reference_output).await?;
            assert_eq!(component_rows(&query).await?, expected);
            assert_eq!(component_rows(&reference).await?, expected);
        }
        query.stop().await?;
        reference.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn synthetic_join_plugin_matches_builtin_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), matches_builtin()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_join_plugin_matches_builtin_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), matches_builtin()).await?
}

fn create_database(path: &Path) -> Result<()> {
    rusqlite::Connection::open(path)?.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, amount INTEGER NOT NULL);
         INSERT INTO customers VALUES (1, 'Ada'), (2, 'Bea');
         INSERT INTO orders VALUES (10, 1, 100), (20, 2, 200);",
    )?;
    Ok(())
}

async fn join_graph(
    database: &Path,
    indexes: &Path,
    generation: u64,
    language: &str,
    joins: &[QueryJoinConfig],
) -> Result<Graph> {
    Graph::with_configuration(
        database,
        indexes,
        generation,
        None,
        json!({
            "query": JOIN_QUERY, "bootstrap": true,
            "language": language, "joins": joins,
            "source_transactions": limits(), "recovery":"strict",
        }),
        vec!["orders".into(), "customers".into()],
    )
    .await
}

async fn bootstrap_live_and_reopen() -> Result<()> {
    for language in ["cypher", "gql"] {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("source.sqlite");
        let indexes = directory.path().join("indexes");
        create_database(&database)?;
        let graph = join_graph(&database, &indexes, 1, language, &join_settings()).await?;
        let first = async {
            graph.start().await?;
            let initial = graph.snapshot().await?;
            assert_eq!(rows(&initial)?, expected(&[(10, 100, "Ada"), (20, 200, "Bea")]));
            assert_eq!(initial.cursor().sequence(), 0);
            for (index, (operations, expected)) in [
                (
                    json!([{"op":"insert","table":"orders","data":{"id":30,"customer_id":1,"amount":300}}]),
                    expected(&[(10, 100, "Ada"), (20, 200, "Bea"), (30, 300, "Ada")]),
                ),
                (
                    json!([{"op":"update","table":"orders","id":"10","data":{"customer_id":2}}]),
                    expected(&[(10, 100, "Bea"), (20, 200, "Bea"), (30, 300, "Ada")]),
                ),
                (
                    json!([{"op":"update","table":"customers","id":"2","data":{"name":"Bex"}}]),
                    expected(&[(10, 100, "Bex"), (20, 200, "Bex"), (30, 300, "Ada")]),
                ),
                (
                    json!([{"op":"delete","table":"customers","id":"1"}]),
                    expected(&[(10, 100, "Bex"), (20, 200, "Bex")]),
                ),
                (
                    json!([{"op":"insert","table":"customers","data":{"id":1,"name":"Cam"}}]),
                    expected(&[(10, 100, "Bex"), (20, 200, "Bex"), (30, 300, "Cam")]),
                ),
                (
                    json!([
                        {"op":"update","table":"orders","id":"30","data":{"customer_id":2,"amount":310}},
                        {"op":"update","table":"customers","id":"2","data":{"name":"Dana"}}
                    ]),
                    expected(&[(10, 100, "Dana"), (20, 200, "Dana"), (30, 310, "Dana")]),
                ),
                (
                    json!([{"op":"delete","table":"orders","id":"20"}]),
                    expected(&[(10, 100, "Dana"), (30, 310, "Dana")]),
                ),
            ]
            .into_iter()
            .enumerate()
            {
                graph.apply(operations).await?;
                let output = graph.output(index).await?;
                assert_eq!(QueryChangeCodec::query_sequence(&output)?, index as u64 + 1);
                assert_eq!(rows(&graph.snapshot().await?)?, expected);
            }
            Ok::<_, anyhow::Error>(initial.cursor().producer().clone())
        }
        .await;
        let stopped = graph.core.shutdown().await;
        let producer = first?;
        stopped?;
        drop(graph);

        let mut reordered = join_settings();
        reordered.reverse();
        for join in &mut reordered {
            join.keys.reverse();
        }
        let graph = join_graph(&database, &indexes, 2, language, &reordered).await?;
        let resumed = async {
            graph.start().await?;
            let snapshot = graph.snapshot().await?;
            assert_eq!(
                rows(&snapshot)?,
                expected(&[(10, 100, "Dana"), (30, 310, "Dana")])
            );
            assert_eq!(snapshot.cursor().producer(), &producer);
            assert_eq!(snapshot.cursor().sequence(), 7);
            assert_eq!(
                graph.progress.snapshot().checkpoints
                    [&SourceProgressKey::Source(graph.source.id().clone())]
                    .sequence,
                7
            );
            graph
                .apply(json!([{"op":"update","table":"orders","id":"10","data":{"amount":111}}]))
                .await?;
            assert_eq!(
                QueryChangeCodec::query_sequence(&graph.output(0).await?)?,
                8
            );
            assert_eq!(
                rows(&graph.snapshot().await?)?,
                expected(&[(10, 111, "Dana"), (30, 310, "Dana")])
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let stopped = graph.core.shutdown().await;
        resumed?;
        stopped?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn synthetic_join_sqlite_bootstrap_live_and_reopen_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), bootstrap_live_and_reopen()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_join_sqlite_bootstrap_live_and_reopen_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), bootstrap_live_and_reopen()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn synthetic_join_changed_settings_cannot_adopt_retained_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    create_database(&database)?;
    let original = join_graph(&database, &indexes, 1, "cypher", &join_settings()).await?;
    original.start().await?;
    let saved = original.snapshot().await?;
    original.core.shutdown().await?;
    drop(original);

    let mut changed = join_settings();
    changed[0].keys[0].property = "amount".into();
    let incompatible = join_graph(&database, &indexes, 2, "cypher", &changed).await?;
    let failure = incompatible.query.start().await;
    let stopped = incompatible.core.shutdown().await;
    let error = failure.expect_err("changed join keys must not reuse retained rows/indexes");
    assert!(format!("{error:#}").contains("configuration"));
    stopped?;
    drop(incompatible);

    let original = join_graph(&database, &indexes, 3, "cypher", &join_settings()).await?;
    original.start().await?;
    let recovered = original.snapshot().await?;
    assert_eq!(rows(&recovered)?, rows(&saved)?);
    assert_eq!(recovered.cursor(), saved.cursor());
    original.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn synthetic_join_auto_reset_preserves_completed_coordinator_ownership() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("source.sqlite");
        let indexes = directory.path().join("indexes");
        create_database(&database)?;
        let original = join_graph(&database, &indexes, 1, "cypher", &join_settings()).await?;
        original.start().await?;
        original.core.shutdown().await?;
        drop(original);

        let mut changed = join_settings();
        changed[0].keys[0].property = "amount".into();
        let graph = Graph::with_configuration(
            &database,
            &indexes,
            2,
            None,
            json!({
                "query":JOIN_QUERY, "bootstrap":true, "language":"cypher",
                "joins":changed, "source_transactions":limits(), "recovery":"auto_reset",
            }),
            vec!["orders".into(), "customers".into()],
        )
        .await?;
        // A completed coordinated handover requires explicit retirement.
        // Query reset must not bypass that contract by discarding ownership state.
        let started = graph.start().await;
        let snapshot = graph.snapshot().await;
        let stopped = graph.core.shutdown().await;
        let error = started.expect_err("completed coordinated handover cannot be replaced");
        assert!(format!("{error:#}").contains("explicit recovery required"));
        assert!(snapshot.is_err());
        stopped?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

fn file_bootstrap(database: &Path, generation: u64) -> Result<Arc<NativeBootstrapProxy>> {
    let plugin = computation::load(sqlite::bootstrap_library())?;
    let factory = plugin
        .bootstrap_factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.sqlite/file-bootstrap"
        })
        .context("missing file bootstrap factory")?;
    let mut configuration = sqlite::provider_configuration(database);
    configuration["settings"]["tables"] = json!(["orders", "customers"]);
    factory.create_query_provider(component_id()?, configuration, scope(generation), None)
}

#[tokio::test(flavor = "current_thread")]
async fn synthetic_join_file_provider_uses_the_same_core_builder() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    create_database(&database)?;
    let mut query = plugin_factory("drasi.experimental/provider-query")?
        .create_component_with_bootstrap(
            component_id()?,
            json!({"query":JOIN_QUERY, "volatile":true, "joins":join_settings()}),
            scope(1),
            Arc::new(NativeBootstrapResource(file_bootstrap(&database, 1)?)),
        )?;
    query.initialize().await?;
    query.start().await?;
    assert_eq!(
        component_rows(&query).await?,
        expected(&[(10, 100, "Ada"), (20, 200, "Bea")])
    );
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn synthetic_join_changed_settings_auto_reset_rebuilds_from_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    create_database(&database)?;
    let factory = plugin_factory("drasi.experimental/indexed-query")?;
    let mut original = factory.create_component_with_processing(
        component_id()?,
        json!({"query":JOIN_QUERY, "bootstrap":true, "joins":join_settings()}),
        scope(1),
        provider(&indexes)?,
    )?;
    original.initialize().await?;
    original.bind_bootstrap(Arc::new(NativeBootstrapResource(file_bootstrap(
        &database, 1,
    )?)))?;
    original.start().await?;
    let saved = reader(&original)?.snapshot().await?;
    assert_eq!(
        rows(&saved)?,
        expected(&[(10, 100, "Ada"), (20, 200, "Bea")])
    );
    original.stop().await?;
    drop(original);

    let mut changed = join_settings();
    changed[0].keys[0].property = "amount".into();
    let mut query = factory.create_component_with_processing(
        component_id()?,
        json!({
            "query":JOIN_QUERY, "bootstrap":true,
            "joins":changed, "recovery":"auto_reset",
        }),
        scope(2),
        provider(&indexes)?,
    )?;
    query.initialize().await?;
    query.bind_bootstrap(Arc::new(NativeBootstrapResource(file_bootstrap(
        &database, 2,
    )?)))?;
    query.start().await?;
    let snapshot = reader(&query)?.snapshot().await?;
    assert!(rows(&snapshot)?.is_empty());
    assert_ne!(snapshot.cursor().producer(), saved.cursor().producer());
    assert_eq!(snapshot.cursor().sequence(), 0);
    assert!(query.continue_transform().await?.is_empty());
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: node(
                    "orders",
                    "new",
                    json!({"id":30,"customer_id":2,"amount":1}),
                    1,
                ),
            },
            1,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    query.delivery_completed(&output).await?;
    assert_eq!(component_rows(&query).await?, expected(&[(30, 1, "Ada")]));
    query.stop().await?;
    Ok(())
}

#[test]
fn synthetic_join_validation_rejects_invalid_settings_at_construction() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let indexes = directory.path().join("unopened-indexes");
    let indexed = plugin_factory("drasi.experimental/indexed-query")?;
    let volatile = plugin_factory("drasi.experimental/volatile-query")?;
    for implementation in [
        "drasi.experimental/volatile-query",
        "drasi.experimental/indexed-query",
        "drasi.experimental/provider-query",
    ] {
        assert_eq!(
            plugin_factory(implementation)?
                .metadata()
                .configuration
                .fields["joins"]
                .value_type,
            drasi_computation_plugin_sdk::ConfigType::Array
        );
    }
    let mut invalid = vec![json!(null), json!({}), json!([{}])];
    let mut duplicate = join_settings();
    duplicate.push(duplicate[0].clone());
    invalid.push(serde_json::to_value(duplicate)?);
    for count in [0, 1] {
        let mut joins = join_settings();
        joins[0].keys.truncate(count);
        invalid.push(serde_json::to_value(joins)?);
    }
    let mut joins = join_settings();
    joins[0].id = "invalid join".into();
    invalid.push(serde_json::to_value(joins)?);
    let mut joins = join_settings();
    joins[0].keys[0].label = String::new();
    invalid.push(serde_json::to_value(joins)?);
    let mut joins = join_settings();
    joins[0].keys[0].property = "invalid property".into();
    invalid.push(serde_json::to_value(joins)?);
    for joins in invalid {
        assert!(indexed
            .create_component_with_processing(
                component_id()?,
                json!({"query":JOIN_QUERY, "joins":joins}),
                scope(1),
                provider(&indexes)?,
            )
            .is_err());
        assert!(volatile
            .create_component(
                component_id()?,
                json!({"query":JOIN_QUERY, "volatile":true, "joins":joins}),
            )
            .is_err());
    }
    assert!(!indexes.exists(), "invalid joins opened query storage");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn synthetic_join_empty_and_omitted_settings_share_retained_identity() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let factory = plugin_factory("drasi.experimental/indexed-query")?;
    let mut configuration = json!({"query":"MATCH (n:Item) RETURN n.value AS value"});
    let mut query = factory.create_component_with_processing(
        component_id()?,
        configuration.clone(),
        scope(1),
        provider(directory.path())?,
    )?;
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
    assert_eq!(output.len(), 1);
    let producer = GraphProducerProgress::from_envelope(&output[0].envelope)?
        .context("missing output producer")?;
    query.delivery_completed(&output).await?;
    query.stop().await?;
    drop(query);

    configuration["joins"] = json!([]);
    let mut query = factory.create_component_with_processing(
        component_id()?,
        configuration,
        scope(2),
        provider(directory.path())?,
    )?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.continue_transform().await?.is_empty());
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: item(12, 2),
            },
            2,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    assert_eq!(
        GraphProducerProgress::from_envelope(&output[0].envelope)?
            .context("missing reopened output producer")?
            .identity(),
        producer.identity()
    );
    match QueryChangeCodec::decode_evaluation(&output[0].envelope)?.as_slice() {
        [QueryPartEvaluationContext::Updating { before, after, .. }] => {
            assert_eq!(before["value"], VariableValue::from(json!(10)));
            assert_eq!(after["value"], VariableValue::from(json!(12)));
        }
        other => anyhow::bail!("unexpected reopened result: {other:?}"),
    }
    query.delivery_completed(&output).await?;
    query.stop().await?;
    Ok(())
}
