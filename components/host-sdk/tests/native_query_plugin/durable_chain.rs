// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::computation::ComputationIndexProvider;

#[path = "durable_rebuild_crash.rs"]
mod rebuild_crash;

struct Child {
    handle: ComponentHandle,
    progress: Arc<QuerySourceProgress>,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
    changed: Arc<Notify>,
}

#[derive(Clone, Copy)]
enum BootstrapInput {
    Resource,
    Edge,
    Unconnected,
}

fn upstream_edge(graph: &Graph, consumer: &ComponentHandle) -> Result<EdgeDefinition> {
    Ok(EdgeDefinition::new(
        Endpoint::new(graph.query.id().clone(), PortId::try_new("out")?),
        Endpoint::new(consumer.id().clone(), PortId::try_new("in")?),
    ))
}

impl Child {
    async fn new(
        graph: &Graph,
        path: &Path,
        input: BootstrapInput,
        query_text: &str,
        recovery: &str,
    ) -> Result<Self> {
        Self::with_indexes(graph, provider(path)?, input, query_text, recovery).await
    }

    async fn with_indexes(
        graph: &Graph,
        indexes: Arc<dyn ComputationIndexProvider>,
        input: BootstrapInput,
        query_text: &str,
        recovery: &str,
    ) -> Result<Self> {
        Self::with_component(
            graph,
            indexes,
            input,
            factory()?,
            json!({"query":query_text, "bootstrap":true, "recovery":recovery,
                "stream":"downstream/out"}),
        )
        .await
    }

    /// The same middle position, held by another stateful transformer on the
    /// runtime the query uses, with its own `settings`. Each passes the
    /// total through unchanged.
    async fn stateful_with_indexes(
        graph: &Graph,
        indexes: Arc<dyn ComputationIndexProvider>,
        input: BootstrapInput,
        recovery: &str,
        (crate_name, implementation, settings): (&str, &str, serde_json::Value),
    ) -> Result<Self> {
        let plugin = computation::load(library().with_file_name(format!(
            "{}{crate_name}{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        )))?;
        let factory = plugin
            .factories()
            .iter()
            .find(|f| f.metadata().implementation.name.as_ref() == implementation)
            .with_context(|| format!("{implementation} factory"))?
            .clone();
        let mut configuration = settings;
        configuration["stream"] = json!("downstream/out");
        configuration["bootstrap"] = json!(true);
        configuration["recovery"] = json!(recovery);
        Self::with_component(graph, indexes, input, factory, configuration).await
    }

    async fn with_component(
        graph: &Graph,
        indexes: Arc<dyn ComputationIndexProvider>,
        input: BootstrapInput,
        factory: Arc<NativeFactory>,
        configuration: serde_json::Value,
    ) -> Result<Self> {
        let scope = Scope {
            instance_id: GRAPH.into(),
            graph_id: graph.core.output_catalogue()?.graph_id().to_string(),
            generation: 1,
        };
        let mut query = factory.create_component_with_processing(
            ComponentId::try_new("8badba5d-be28-5339-a4de-413fc739c325")?,
            configuration,
            scope,
            indexes,
        )?;
        let progress = query
            .processing_progress_owner()
            .context("missing downstream progress")?;
        if matches!(input, BootstrapInput::Resource) {
            let bootstrap = OutputBootstrapProvider::new(
                graph.core.output_catalogue()?,
                vec![OutputEndpoint::new(
                    graph.query.id().clone(),
                    PortId::try_new("out")?,
                )],
                progress.clone(),
                Duration::from_secs(5),
            )?;
            query.bind_bootstrap_provider(Arc::new(NativeBootstrapResource(bootstrap)))?;
        }
        let handle = graph
            .core
            .add_computation_component(
                ComponentAddition::new(
                    drasi_lib::computation::v1::ComponentName::try_new("downstream")
                        .expect("fixture component name"),
                    ConstructedComponent::transformer(Box::new(query)),
                )
                .auto_start(false)
                .bind_stream(
                    PortId::try_new("out")?,
                    StreamId::try_new("downstream/out")?,
                ),
            )
            .await?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(Notify::new());
        let sink = graph
            .core
            .add_computation_component(
                ComponentAddition::new(
                    drasi_lib::computation::v1::ComponentName::try_new("downstream-results")
                        .expect("fixture component name"),
                    ConstructedComponent::sink(Box::new(Sink {
                        descriptor: ComponentDescriptor::try_new(
                            ComponentId::try_new("6f9b567b-37a1-5cf5-ba29-fa86ef698e32")?,
                            vec![port_descriptor("in", PortDirection::Input)?],
                        )?,
                        received: received.clone(),
                        notification: changed.clone(),
                    })),
                )
                .auto_start(false),
            )
            .await?;
        // The data edge owns the internal state lane. In the Edge case the
        // downstream has neither a separately configured edge nor a provider.
        let mut edges = Vec::new();
        if !matches!(input, BootstrapInput::Unconnected) {
            edges.push(upstream_edge(graph, &handle)?);
        }
        edges.push(EdgeDefinition::new(
            Endpoint::new(handle.id().clone(), PortId::try_new("out")?),
            Endpoint::new(sink.id().clone(), PortId::try_new("in")?),
        ));
        crate::bootstrap::connect_relationships(
            &graph.core,
            edges.into_iter().map(|definition| {
                let resource = matches!(input, BootstrapInput::Resource)
                    && &definition.to.component == handle.id();
                DesiredRelationship {
                    definition,
                    policy: RelationshipPolicy {
                        initial_state: resource.then_some(false),
                        ..Default::default()
                    },
                    pipe: DesiredPipe::Bounded { capacity: 8 },
                }
            }),
        )
        .await?;
        sink.start().await?;
        Ok(Self {
            handle,
            progress,
            received,
            changed,
        })
    }

    async fn output(&self, index: usize) -> Result<ChangeEnvelope> {
        loop {
            let changed = self.changed.notified();
            if let Some(output) = self
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("child output capture poisoned"))?
                .get(index)
                .cloned()
            {
                return Ok(output);
            }
            changed.await;
        }
    }

    async fn snapshot(&self, graph: &Graph) -> Result<OutputSnapshot> {
        Ok(graph
            .core
            .output_catalogue()?
            .resolve(&OutputEndpoint::new(
                self.handle.id().clone(),
                PortId::try_new("out")?,
            ))?
            .snapshot()
            .await?)
    }
}

const SUM: &str = "MATCH (n) RETURN sum(n.total) AS total";

/// The stateful transformer in the middle of a chain: the query, the
/// stateful middleware, the view or geometry, all on the same transformer
/// runtime.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Middle {
    Query,
    Middleware,
    View,
    Geometry,
}

/// Relabel each result row.
fn relabel() -> serde_json::Value {
    json!({"middleware":[{"name":"rename","kind":"relabel",
        "config":{"labelMappings":{"Result":"Mapped"}}}],
        "pipeline":["rename"]})
}

/// The total row is a point in two sets that intersects itself, so the one
/// relation record carries the total.
fn intersect_total() -> serde_json::Value {
    let point = json!({"type":"point","x":"total","y":"items"});
    json!({
        "sets":[{"name":"left","label":"Result","geometry":point},
                {"name":"right","label":"Result","geometry":point}],
        "relations":[{"name":"same","left":"left","right":"right","predicate":"intersects",
                      "label":"Mapped","copyLeft":{"total":"total"}}]
    })
}

impl Middle {
    /// The upstream graph. Geometry's point needs a second coordinate, so its
    /// upstream also returns the item count.
    async fn graph(self, database: &Path, indexes: &Path, generation: u64) -> Result<Graph> {
        if self != Self::Geometry {
            return Graph::new(database, indexes, generation).await;
        }
        Graph::with_configuration(
            database,
            indexes,
            generation,
            None,
            json!({"query":"MATCH (n:items) RETURN sum(n.amount) AS total, count(n) AS items",
                "bootstrap":true, "sourceTransactions":crate::transaction_settings(limits()),
                "recovery":"strict"}),
            vec!["items".into()],
        )
        .await
    }

    /// The single total in the middle's current state. Only the query's
    /// rows are query results; the others publish plain elements.
    fn total(self, snapshot: &OutputSnapshot) -> Result<f64> {
        if self == Self::Query {
            return total(snapshot);
        }
        let mut records = snapshot.records().values();
        let record = records.next().context("missing total record")?;
        anyhow::ensure!(records.next().is_none(), "expected one total record");
        let image = GraphChangeCodec::decode_element(record)?;
        let values = drasi_core_types::data_value::decode_variables(image.properties().clone())?;
        QueryChangeCodec::row_values_to_json(&values)["total"]
            .as_f64()
            .context("missing total")
    }

    fn name(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Middleware => "middleware",
            Self::View => "view",
            Self::Geometry => "geometry",
        }
    }

    /// The loaded library, implementation and settings of a non-query middle.
    fn stateful(self) -> (&'static str, &'static str, serde_json::Value) {
        match self {
            Self::Query => unreachable!("the query is created through its own factory"),
            Self::Middleware => (
                "drasi_computation_transformer_middleware",
                "drasi.standard/stateful-middleware",
                relabel(),
            ),
            Self::View => (
                "drasi_computation_transformer_view",
                "drasi.standard/view",
                relabel(),
            ),
            Self::Geometry => (
                "drasi_computation_transformer_geometry",
                "drasi.standard/geometry",
                intersect_total(),
            ),
        }
    }

    async fn child(
        self,
        graph: &Graph,
        indexes: Arc<dyn ComputationIndexProvider>,
    ) -> Result<Child> {
        self.child_with(graph, indexes, BootstrapInput::Edge, "auto_reset")
            .await
    }

    async fn child_with(
        self,
        graph: &Graph,
        indexes: Arc<dyn ComputationIndexProvider>,
        input: BootstrapInput,
        recovery: &str,
    ) -> Result<Child> {
        match self {
            Self::Query => Child::with_indexes(graph, indexes, input, SUM, recovery).await,
            _ => {
                Child::stateful_with_indexes(graph, indexes, input, recovery, self.stateful()).await
            }
        }
    }
}

/// The single changed row's total in a stateful middleware output, and its
/// position in the middleware's output history.
fn middleware_change(output: &ChangeEnvelope) -> Result<(f64, u64)> {
    let [ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. }] =
        output.changes().operations()
    else {
        anyhow::bail!(
            "expected one changed row: {:?}",
            output.changes().operations()
        );
    };
    let image = GraphChangeCodec::decode_element(after)?;
    let values = drasi_core_types::data_value::decode_variables(image.properties().clone())?;
    Ok((
        QueryChangeCodec::row_values_to_json(&values)["total"]
            .as_f64()
            .context("missing total")?,
        GraphProducerProgress::from_envelope(output)?
            .context("middleware output has no position")?
            .sequence(),
    ))
}

async fn add_leaf(graph: &Graph, middle: &Child) -> Result<ComponentHandle> {
    let catalogue = graph.core.output_catalogue()?;
    let out = PortId::try_new("out")?;
    let leaf_id = ComponentId::try_new("ee920ded-3dc2-43e8-8733-ab63378d28a9")?;
    let leaf_stream = StreamId::try_new("leaf/out")?;
    let leaf = factory()?.create_component_with_processing(
        leaf_id.clone(),
        json!({"query": "MATCH (n) RETURN n.total AS total",
            "bootstrap":true, "volatile":true, "stream":leaf_stream}),
        Scope {
            instance_id: GRAPH.into(),
            graph_id: catalogue.graph_id().to_string(),
            generation: 1,
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )?;
    let leaf = graph
        .core
        .add_computation_component(
            ComponentAddition::new(
                ComponentName::try_new("leaf")?,
                ConstructedComponent::transformer(Box::new(leaf)),
            )
            .auto_start(false)
            .bind_stream(out.clone(), leaf_stream),
        )
        .await?;
    crate::bootstrap::connect_with_capacity(
        &graph.core,
        vec![EdgeDefinition::new(
            Endpoint::new(middle.handle.id().clone(), out),
            Endpoint::new(leaf_id, PortId::try_new("in")?),
        )],
        8,
    )
    .await?;
    Ok(leaf)
}

async fn miss_two_updates(graph: &Graph, middle: &Child) -> Result<()> {
    middle.handle.stop().await?;
    for (index, amount) in [(0, 11), (1, 12)] {
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":amount}}]))
            .await?;
        graph.output(index).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middle_query_refuses_a_gap_reset_that_would_strand_a_running_dependent() -> Result<()> {
    quiet_gap(BootstrapInput::Resource).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middle_query_rebuild_corrects_a_running_leaf_on_a_quiet_stream() -> Result<()> {
    quiet_gap(BootstrapInput::Edge).await
}

#[tokio::test(flavor = "current_thread")]
async fn middle_rebuild_publishes_only_net_keyed_changes_even_when_the_difference_is_empty(
) -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::with_configuration(
        &database,
        &root.path().join("upstream"),
        1,
        None,
        json!({"query":"MATCH (n:items) RETURN n.amount AS total","bootstrap":true,
            "sourceTransactions":crate::transaction_settings(limits()), "outboxCapacity":1}),
        vec!["items".into()],
    )
    .await?;
    let run = tokio::time::timeout(Duration::from_secs(30), async {
        graph.start().await?;
        let middle = Child::new(
            &graph,
            &root.path().join("middle"),
            BootstrapInput::Edge,
            "MATCH (n) RETURN n.total AS total",
            "auto_reset",
        )
        .await?;
        middle.handle.start().await?;
        let leaf = add_leaf(&graph, &middle).await?;
        leaf.start().await?;
        let out = graph
            .core
            .output_catalogue()?
            .resolve(&OutputEndpoint::new(
                leaf.id().clone(),
                PortId::try_new("out")?,
            ))?;
        let (initial, mut follow) = out
            .snapshot_and_follow(drasi_computation_contracts::OutputFollowLimits {
                max_envelopes: 1.try_into()?,
                max_bytes: (1 << 20).try_into()?,
                lifetime: Duration::from_secs(5),
            })
            .await?
            .into_parts();
        assert_eq!(initial.records().len(), 2);
        let cut = middle.snapshot(&graph).await?.cursor().clone();
        middle.handle.stop().await?;
        for (i, operation) in [
            json!({"op":"delete","table":"items","id":"2"}),
            json!({"op":"update","table":"items","id":"1","data":{"amount":12}}),
            json!({"op":"insert","table":"items","data":{"id":3,"amount":40}}),
        ]
        .into_iter()
        .enumerate()
        {
            graph.apply(json!([operation])).await?;
            graph.output(i).await?;
        }
        middle.handle.start().await?;
        let output = middle.output(0).await?;
        let mut changes = Vec::new();
        for change in QueryChangeCodec::decode_evaluation(&output)? {
            use QueryPartEvaluationContext::*;
            let amount = |v: &drasi_core::evaluation::context::QueryVariables| {
                QueryChangeCodec::row_values_to_json(v)["total"]
                    .as_i64()
                    .context("total must be an integer")
            };
            changes.push(match change {
                Adding { after, .. } => (None, Some(amount(&after)?)),
                Updating { before, after, .. } => (Some(amount(&before)?), Some(amount(&after)?)),
                Removing { before, .. } => (Some(amount(&before)?), None),
                _ => anyhow::bail!("unexpected operation in state difference"),
            });
        }
        changes.sort();
        assert_eq!(
            changes,
            [(None, Some(40)), (Some(10), Some(12)), (Some(20), None)]
        );
        follow.next().await?;
        let observed = out.snapshot().await?;
        let mut totals: Vec<_> = observed
            .records()
            .values()
            .map(|r| -> Result<i64> {
                QueryChangeCodec::row_values_to_json(&QueryChangeCodec::decode_row(r)?.values)
                    ["total"]
                    .as_i64()
                    .context("expected row")
            })
            .collect::<Result<_>>()?;
        totals.sort();
        assert_eq!(totals, [12, 40]);
        middle.handle.stop().await?;
        for (i, amount) in [(3, 13), (4, 12)] {
            graph
                .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":amount}}]))
                .await?;
            graph.output(i).await?;
        }
        middle.handle.start().await?;
        let key = SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?);
        assert_eq!(middle.progress.snapshot().checkpoints[&key].sequence, 5);
        assert_eq!(middle.snapshot(&graph).await?.cursor(), &cut.at_sequence(1));
        assert_eq!(out.snapshot().await?.cursor(), observed.cursor());
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let stopped = graph.core.shutdown().await;
    run??;
    stopped?;
    Ok(())
}

async fn quiet_gap(input: BootstrapInput) -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::new(&database, &root.path().join("upstream"), 1).await?;
    let exercise = tokio::time::timeout(Duration::from_secs(30), async {
        graph.start().await?;
        let middle = Child::new(
            &graph,
            &root.path().join("middle"),
            input,
            SUM,
            "auto_reset",
        )
        .await?;
        middle.handle.start().await?;
        let catalogue = graph.core.output_catalogue()?;
        let leaf = add_leaf(&graph, &middle).await?;
        leaf.start().await?;
        let leaf_output = OutputEndpoint::new(leaf.id().clone(), PortId::try_new("out")?);
        let (initial, mut follow) = catalogue
            .resolve(&leaf_output)?
            .snapshot_and_follow(drasi_computation_contracts::OutputFollowLimits {
                max_envelopes: 1.try_into()?,
                max_bytes: (1 << 20).try_into()?,
                lifetime: Duration::from_secs(5),
            })
            .await?
            .into_parts();
        assert_eq!(total(&initial)?, 30.0);
        let middle_cut = middle.snapshot(&graph).await?.cursor().clone();
        let before = middle.progress.snapshot();
        let key = SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?);
        // This fixture retains one query result. Both writes commit upstream
        // while the leaf is still running, and no new writes follow the restart.
        miss_two_updates(&graph, &middle).await?;
        let restart = middle.handle.start().await;
        if matches!(input, BootstrapInput::Edge) {
            restart?;
            let change = follow.next().await?;
            assert_eq!(QueryChangeCodec::query_sequence(&change)?, 1);
            let retained = catalogue.resolve(&leaf_output)?.snapshot().await?;
            assert_eq!(retained.cursor(), &initial.cursor().at_sequence(1));
            assert_eq!(total(&retained)?, 32.0);
            let rebuilt = middle.snapshot(&graph).await?;
            assert_eq!(rebuilt.cursor(), &middle_cut.at_sequence(1));
            assert_eq!(total(&rebuilt)?, 32.0);
            let after = middle.progress.snapshot();
            assert_eq!(before.reset_generation, after.reset_generation);
            assert_eq!(after.checkpoints[&key].sequence, 2);
            return Ok(());
        }
        let refused =
            restart.expect_err("an unqualified replacement must not leave the leaf stale");
        let reason = format!("{refused:#}");
        anyhow::ensure!(
            reason.contains("downstream state") && reason.contains(leaf.id().as_str()),
            "{reason}"
        );
        let after = middle.progress.snapshot();
        assert_eq!(before.reset_generation, after.reset_generation);
        assert_eq!(
            before.checkpoints[&key].sequence,
            after.checkpoints[&key].sequence
        );
        let retained = catalogue.resolve(&leaf_output)?.snapshot().await?;
        assert_eq!(retained.cursor(), initial.cursor());
        assert_eq!(total(&retained)?, 30.0);
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let stopped = graph.core.shutdown().await;
    exercise??;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_input_reaches_a_dependent_that_installed_the_empty_view() -> Result<()> {
    first_input(Middle::Query).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_input_reaches_a_dependent_of_an_empty_stateful_middleware() -> Result<()> {
    first_input(Middle::Middleware).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_input_reaches_a_dependent_of_an_empty_view() -> Result<()> {
    first_input(Middle::View).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_input_reaches_a_dependent_of_an_empty_geometry() -> Result<()> {
    first_input(Middle::Geometry).await
}

async fn first_input(kind: Middle) -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = kind
        .graph(&database, &root.path().join("upstream"), 1)
        .await?;
    let run = tokio::time::timeout(Duration::from_secs(30), async {
        graph.start().await?;
        // Strict: reading a first input is initialization, not a recovery reset.
        let middle = kind
            .child_with(
                &graph,
                provider(&root.path().join("middle"))?,
                BootstrapInput::Unconnected,
                "strict",
            )
            .await?;
        middle.handle.start().await?;
        let empty = middle.snapshot(&graph).await?;
        anyhow::ensure!(empty.records().is_empty() && empty.cursor().sequence() == 0);
        let leaf = add_leaf(&graph, &middle).await?;
        leaf.start().await?;
        let leaf_output = graph
            .core
            .output_catalogue()?
            .resolve(&OutputEndpoint::new(
                leaf.id().clone(),
                PortId::try_new("out")?,
            ))?;
        let (initial, mut follow) = leaf_output
            .snapshot_and_follow(drasi_computation_contracts::OutputFollowLimits {
                max_envelopes: 1.try_into()?,
                max_bytes: (1 << 20).try_into()?,
                lifetime: Duration::from_secs(5),
            })
            .await?
            .into_parts();
        assert!(initial.records().is_empty());

        // Connecting the first input quiesces this subgraph; the leaf's
        // follow lease stays open through that pause.
        crate::bootstrap::connect_with_capacity(
            &graph.core,
            vec![upstream_edge(&graph, &middle.handle)?],
            8,
        )
        .await?;
        middle.handle.wait_started().await?;
        // No source write follows. The leaf must still move from no rows to 30.
        let change = follow.next().await?;
        assert_eq!(QueryChangeCodec::query_sequence(&change)?, 1);
        let seeded_leaf = leaf_output.snapshot().await?;
        assert_eq!(seeded_leaf.cursor(), &initial.cursor().at_sequence(1));
        assert_eq!(total(&seeded_leaf)?, 30.0);
        let seeded = middle.snapshot(&graph).await?;
        assert_eq!(seeded.cursor(), &empty.cursor().at_sequence(1));
        assert_eq!(kind.total(&seeded)?, 30.0);
        let first = middle.output(0).await?;
        if kind == Middle::Query {
            let rows = QueryChangeCodec::decode_evaluation(&first)?;
            let [QueryPartEvaluationContext::Adding { after, .. }] = rows.as_slice() else {
                anyhow::bail!("expected the first input's row as one addition: {rows:?}");
            };
            assert_eq!(
                QueryChangeCodec::row_values_to_json(after)["total"].as_f64(),
                Some(30.0)
            );
        } else {
            anyhow::ensure!(
                matches!(
                    first.changes().operations(),
                    [ChangeOperation::Added { .. }]
                ),
                "expected the first input's row as one addition"
            );
            assert_eq!(middleware_change(&first)?, (30.0, 1));
        }
        let key = SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?);
        let progress = middle.progress.snapshot();
        assert_eq!(progress.checkpoints[&key].sequence, 0);
        assert_eq!(progress.reset_generation, 0);

        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":12}}]))
            .await?;
        let live = middle.output(1).await?;
        if kind == Middle::Query {
            aggregate(
                &[OutputEnvelope::new(PortId::try_new("out")?, live)],
                Some(30.0),
                32.0,
                2,
            )?;
        } else {
            assert_eq!(middleware_change(&live)?, (32.0, 2));
        }
        follow.next().await?;
        assert_eq!(total(&leaf_output.snapshot().await?)?, 32.0);
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let stopped = graph.core.shutdown().await;
    run??;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_reopen_distinguishes_a_first_input_from_an_input_with_saved_progress() -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let key = SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?);
    let mut vacant = None;
    let mut seeded = None;

    for generation in 1..=3 {
        let graph = Graph::new(&database, &root.path().join("upstream"), generation).await?;
        let run = tokio::time::timeout(Duration::from_secs(30), async {
            let child = Child::new(
                &graph,
                &root.path().join("downstream"),
                BootstrapInput::Unconnected,
                SUM,
                "strict",
            )
            .await?;
            child.handle.start().await?;
            let before = child.snapshot(&graph).await?;
            if generation <= 2 {
                anyhow::ensure!(before.records().is_empty() && before.cursor().sequence() == 0);
                anyhow::ensure!(
                    child.progress.snapshot().checkpoints.is_empty(),
                    "unconnected start fabricated an input checkpoint"
                );
                if generation == 1 {
                    vacant = Some(before.cursor().clone());
                    return Ok::<_, anyhow::Error>(());
                }
                assert_eq!(
                    Some(before.cursor()),
                    vacant.as_ref(),
                    "cold open replaced the unconnected view"
                );
            } else {
                assert_eq!(
                    Some(before.cursor()),
                    seeded.as_ref(),
                    "disconnect discarded completed state"
                );
                assert_eq!(total(&before)?, 32.0);
                assert_eq!(child.progress.snapshot().checkpoints[&key].sequence, 1);
            }

            graph.start().await?;
            if generation == 3 {
                graph
                    .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":13}}]))
                    .await?;
                graph.output(0).await?;
            }
            crate::bootstrap::connect_with_capacity(
                &graph.core,
                vec![upstream_edge(&graph, &child.handle)?],
                8,
            )
            .await?;
            child.handle.wait_started().await?;
            if generation == 2 {
                let initial = child.snapshot(&graph).await?;
                assert_eq!(total(&initial)?, 30.0);
                assert_ne!(initial.cursor().producer(), before.cursor().producer());
                assert_eq!(child.progress.snapshot().checkpoints[&key].sequence, 0);
                graph
                    .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":12}}]))
                    .await?;
                let output = child.output(0).await?;
                aggregate(
                    &[OutputEnvelope::new(PortId::try_new("out")?, output)],
                    Some(30.0),
                    32.0,
                    1,
                )?;
                seeded = Some(child.snapshot(&graph).await?.cursor().clone());
            } else {
                let output = child.output(0).await?;
                aggregate(
                    &[OutputEnvelope::new(PortId::try_new("out")?, output)],
                    Some(32.0),
                    33.0,
                    2,
                )?;
                let restored = child.snapshot(&graph).await?;
                assert_eq!(restored.cursor().producer(), before.cursor().producer());
                assert_eq!(total(&restored)?, 33.0);
                assert_eq!(child.progress.snapshot().checkpoints[&key].sequence, 2);
            }
            assert_eq!(child.progress.snapshot().reset_generation, 0);
            Ok(())
        })
        .await;
        let stopped = graph.core.shutdown().await;
        run.with_context(|| format!("cold open {generation} timed out"))??;
        stopped?;
    }
    Ok(())
}

async fn replay_and_gap(input: BootstrapInput) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::new(&database, &directory.path().join("upstream"), 1).await?;
    let exercise = async {
        graph.start().await?;
        let child = Child::new(
            &graph,
            &directory.path().join("downstream"),
            input,
            SUM,
            "auto_reset",
        )
        .await?;
        child
            .handle
            .start()
            .await
            .context("initial downstream bootstrap")?;
        let initial = child.snapshot(&graph).await?;
        assert_eq!(total(&initial)?, 30.0);
        assert_eq!(initial.cursor().sequence(), 0);
        let producer = initial.cursor().producer().clone();
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":12}}]))
            .await?;
        let output = child.output(0).await?;
        aggregate(
            &[OutputEnvelope {
                execution_context: Default::default(),
                port: PortId::try_new("out")?,
                envelope: output,
            }],
            Some(30.0),
            32.0,
            1,
        )?;
        child.handle.stop().await?;
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":13}}]))
            .await?;
        graph.output(1).await?;
        child
            .handle
            .start()
            .await
            .context("downstream replay restart")?;
        let replayed = child.output(1).await?;
        aggregate(
            &[OutputEnvelope {
                execution_context: Default::default(),
                port: PortId::try_new("out")?,
                envelope: replayed,
            }],
            Some(32.0),
            33.0,
            2,
        )?;
        assert_eq!(child.snapshot(&graph).await?.cursor().producer(), &producer);
        child.handle.stop().await?;
        for (index, amount) in [(2, 14), (3, 15)] {
            graph
                .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":amount}}]))
                .await?;
            graph.output(index).await?;
        }
        child
            .handle
            .start()
            .await
            .context("downstream retention-gap reset")?;
        let reset = child.snapshot(&graph).await?;
        assert_eq!(total(&reset)?, 35.0);
        assert_ne!(reset.cursor().producer(), &producer);
        assert_eq!(
            reset.cursor().sequence(),
            2,
            "reset must retain the logical high-water mark"
        );
        assert_eq!(child.progress.snapshot().reset_generation, 1);
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":16}}]))
            .await?;
        let output = child.output(2).await?;
        assert_eq!(
            output.system().sequence(),
            3,
            "reset must retain the transport high-water mark"
        );
        aggregate(
            &[OutputEnvelope {
                execution_context: Default::default(),
                port: PortId::try_new("out")?,
                envelope: output,
            }],
            Some(35.0),
            36.0,
            3,
        )?;
        let progress = child.progress.snapshot();
        assert_eq!(
            progress.checkpoints
                [&SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?)]
                .sequence,
            5
        );
        assert_eq!(
            child
                .received
                .lock()
                .expect("child result capture poisoned")
                .len(),
            3,
            "buffered duplicates were evaluated again"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

async fn waits_for_upstream() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::new(&database, &directory.path().join("upstream"), 1).await?;
    let exercise = async {
        let child = Child::new(&graph, &directory.path().join("downstream"), BootstrapInput::Resource, SUM, "auto_reset").await?;
        let mut progress = child.progress.subscribe();
        let start = child.handle.start();
        tokio::pin!(start);
        tokio::select! {
            result = &mut start => anyhow::bail!("downstream started before its upstream: {result:?}"),
            result = async {
                while !progress.borrow_and_update().recovered { progress.changed().await?; }
                Ok::<_, anyhow::Error>(())
            } => result?,
        }
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut start).await.is_err());
        assert!(child.snapshot(&graph).await.is_err());
        let before = graph.query.observed()?;
        graph.start().await?;
        start.await.with_context(|| {
            let after = graph.query.observed().expect("upstream observation");
            format!("upstream initialization changed {:?} to {:?}",
                (before.generation, before.operation, before.lifecycle, before.realization),
                (after.generation, after.operation, after.lifecycle, after.realization))
        })?;
        assert_eq!(total(&child.snapshot(&graph).await?)?, 30.0);
        Ok::<_, anyhow::Error>(())
    }.await;
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_chain_waits_for_upstream_initialization_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), waits_for_upstream()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_chain_waits_for_upstream_initialization_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), waits_for_upstream()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_chain_bootstrap_replay_and_gap_reset_current_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(40),
        replay_and_gap(BootstrapInput::Resource),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_chain_bootstrap_replay_and_gap_reset_multi_thread() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(40),
        replay_and_gap(BootstrapInput::Resource),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_edge_restart_replays_then_auto_resets_only_on_a_gap() -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(40),
        replay_and_gap(BootstrapInput::Edge),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_edge_restart_recovers_missed_update_and_delete_before_live() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::with_configuration(
        &database,
        &directory.path().join("upstream"),
        1,
        None,
        json!({"query":"MATCH (n:items) RETURN n.amount AS total", "bootstrap":true,
            "sourceTransactions":crate::transaction_settings(limits()), "outboxCapacity":2}),
        vec!["items".into()],
    )
    .await?;
    let exercise = tokio::time::timeout(Duration::from_secs(40), async {
        let child = Child::new(
            &graph,
            &directory.path().join("downstream"),
            BootstrapInput::Edge,
            "MATCH (n) RETURN n.total AS total",
            "strict",
        )
        .await?;
        graph.start().await?;
        child.handle.start().await?;
        let initial = child.snapshot(&graph).await?;
        assert_eq!(initial.records().len(), 2);
        let key = SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?);
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":11}}]))
            .await?;
        child.output(0).await?;
        child.handle.stop().await?;

        // Drop the data edge while the consumer is down. Neither change
        // can be waiting in its volatile queue; rebind before restarting.
        let live = upstream_edge(&graph, &child.handle)?;
        let committed = graph.core.desired_configuration()?;
        let mut desired = committed.desired;
        let relationships = &mut desired.topology.relationships;
        let index = relationships
            .iter()
            .position(|r| r.definition == live)
            .context("missing live edge")?;
        relationships.remove(index);
        graph
            .core
            .apply_desired_state(committed.revision, "disconnect-offline-consumer", desired)
            .await?;
        let report = graph.core.reconcile_desired_state().await?;
        anyhow::ensure!(report.applied && report.error.is_none(), "{report:?}");

        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":12}}]))
            .await?;
        let updated = graph.output(1).await?;
        graph
            .apply(json!([{"op":"delete","table":"items","id":"2"}]))
            .await?;
        let deleted = graph.output(2).await?;
        assert_eq!(child.progress.snapshot().checkpoints[&key].sequence, 1);

        crate::bootstrap::connect_with_capacity(&graph.core, vec![live], 8).await?;
        child
            .handle
            .start()
            .await
            .context("recover downstream over snapshot edge")?;
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":13}}]))
            .await?;
        let live = graph.output(3).await?;
        for (i, original) in [(1, updated), (2, deleted), (3, live)] {
            let output = child.output(i).await?;
            assert_eq!(QueryChangeCodec::query_sequence(&output)?, i as u64 + 1);
            let parent = output
                .lineage()
                .context("recovered change lost its input identity")?;
            assert_eq!(parent.envelope_id(), original.id());
            assert_eq!(parent.system(), original.system());
            let changes = QueryChangeCodec::decode_evaluation(&output)?;
            match (i, changes.as_slice()) {
                (2, [QueryPartEvaluationContext::Removing { before, .. }]) => {
                    assert_eq!(before["total"].as_f64(), Some(20.0));
                }
                (1 | 3, [QueryPartEvaluationContext::Updating { before, after, .. }]) => {
                    let value = if i == 1 { 12.0 } else { 13.0 };
                    assert_eq!(before["total"].as_f64(), Some(value - 1.0));
                    assert_eq!(after["total"].as_f64(), Some(value));
                }
                _ => anyhow::bail!("unexpected downstream change {i}: {changes:?}"),
            }
        }
        let recovered = child.snapshot(&graph).await?;
        assert_eq!(total(&recovered)?, 13.0);
        assert_eq!(recovered.cursor().producer(), initial.cursor().producer());
        assert_eq!(child.progress.snapshot().checkpoints[&key].sequence, 4);
        assert_eq!(child.progress.snapshot().reset_generation, 0);
        child.handle.stop().await?;
        // Capacity is two. Strict cannot install a fresh baseline or advance
        // past the evicted update, even if the live queue overlaps the gap.
        for (i, amount) in [(4, 14), (5, 15), (6, 16)] {
            graph
                .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":amount}}]))
                .await?;
            graph.output(i).await?;
        }
        let error = child
            .handle
            .start()
            .await
            .expect_err("strict retention gap");
        assert!(
            format!("{error:#}").contains("output history after 4 is unavailable"),
            "{error:#}"
        );
        assert_eq!(child.progress.snapshot().reset_generation, 0);
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let stopped = graph.core.shutdown().await;
    exercise??;
    stopped?;
    Ok(())
}

/// A stateful middleware or view in the middle of a persistent chain behaves
/// as the query does there: it bootstraps from its upstream, replays a missed
/// change on restart, and after a history gap either rebuilds for its running
/// dependent (data edge) or refuses a reset that would strand it (provider).
async fn stateful_in_the_middle(kind: Middle, input: BootstrapInput) -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = kind
        .graph(&database, &root.path().join("upstream"), 1)
        .await?;
    let exercise = tokio::time::timeout(Duration::from_secs(30), async {
        graph.start().await?;
        let middle = kind
            .child_with(&graph, provider(&root.path().join("middle"))?, input, "auto_reset")
            .await?;
        middle.handle.start().await?;
        let installed = middle.snapshot(&graph).await?;
        assert_eq!(kind.total(&installed)?, 30.0);
        assert!(installed.cursor().producer().persistent());
        let catalogue = graph.core.output_catalogue()?;
        let leaf = add_leaf(&graph, &middle).await?;
        leaf.start().await?;
        let leaf_output = OutputEndpoint::new(leaf.id().clone(), PortId::try_new("out")?);
        let leaf_total = || async {
            total(&catalogue.resolve(&leaf_output)?.snapshot().await?)
        };
        let (initial, mut follow) = catalogue
            .resolve(&leaf_output)?
            .snapshot_and_follow(drasi_computation_contracts::OutputFollowLimits {
                max_envelopes: 1.try_into()?,
                max_bytes: (1 << 20).try_into()?,
                lifetime: Duration::from_secs(10),
            })
            .await?
            .into_parts();
        assert_eq!(total(&initial)?, 30.0);
        let update = |amount: i64| {
            json!([{"op":"update","table":"items","id":"1","data":{"amount":amount}}])
        };

        graph.apply(update(11)).await?;
        graph.output(0).await?;
        middle.output(0).await?;
        follow.next().await?;
        assert_eq!(leaf_total().await?, 31.0);

        // Upstream retains one output, so a single missed change replays.
        middle.handle.stop().await?;
        graph.apply(update(12)).await?;
        graph.output(1).await?;
        middle.handle.start().await.context("middleware replay restart")?;
        middle.output(1).await?;
        follow.next().await?;
        assert_eq!(leaf_total().await?, 32.0);
        let replayed = middle.snapshot(&graph).await?;
        assert_eq!(replayed.cursor(), &installed.cursor().at_sequence(2));

        // Two missed changes are a gap in that retained history.
        let key = SourceProgressKey::Stream(StreamId::try_new(format!("{COMPONENT}/out"))?);
        let before = middle.progress.snapshot();
        middle.handle.stop().await?;
        for (index, amount) in [(2, 13), (3, 14)] {
            graph.apply(update(amount)).await?;
            graph.output(index).await?;
        }
        let restart = middle.handle.start().await;
        if matches!(input, BootstrapInput::Edge) {
            restart.context("middleware rebuild restart")?;
            follow.next().await?;
            assert_eq!(leaf_total().await?, 34.0);
            let rebuilt = middle.snapshot(&graph).await?;
            assert_eq!(rebuilt.cursor(), &installed.cursor().at_sequence(3));
            assert_eq!(kind.total(&rebuilt)?, 34.0);
            let after = middle.progress.snapshot();
            assert_eq!(before.reset_generation, after.reset_generation);
            assert_eq!(after.checkpoints[&key].sequence, 4);
            return Ok(());
        }
        let refused = restart.expect_err("a provider reset must not leave the leaf stale");
        let reason = format!("{refused:#}");
        anyhow::ensure!(
            reason.contains("downstream state") && reason.contains(leaf.id().as_str()),
            "{reason}"
        );
        assert_eq!(
            before.checkpoints[&key].sequence,
            middle.progress.snapshot().checkpoints[&key].sequence
        );
        assert_eq!(leaf_total().await?, 32.0);
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let stopped = graph.core.shutdown().await;
    exercise??;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middle_stateful_middleware_replays_then_rebuilds_for_a_running_leaf() -> Result<()> {
    stateful_in_the_middle(Middle::Middleware, BootstrapInput::Edge).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middle_stateful_middleware_refuses_a_gap_reset_that_would_strand_its_leaf() -> Result<()> {
    stateful_in_the_middle(Middle::Middleware, BootstrapInput::Resource).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middle_view_replays_then_rebuilds_for_a_running_leaf() -> Result<()> {
    stateful_in_the_middle(Middle::View, BootstrapInput::Edge).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middle_view_refuses_a_gap_reset_that_would_strand_its_leaf() -> Result<()> {
    stateful_in_the_middle(Middle::View, BootstrapInput::Resource).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middle_geometry_replays_then_rebuilds_for_a_running_leaf() -> Result<()> {
    stateful_in_the_middle(Middle::Geometry, BootstrapInput::Edge).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn middle_geometry_refuses_a_gap_reset_that_would_strand_its_leaf() -> Result<()> {
    stateful_in_the_middle(Middle::Geometry, BootstrapInput::Resource).await
}
