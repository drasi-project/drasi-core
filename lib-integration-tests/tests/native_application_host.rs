// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_bootstrap_application::ApplicationBootstrapProvider;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::{computation::v1::*, DrasiLib};
use drasi_reaction_application::NativeApplicationReaction;
use drasi_source_application::{
    ApplicationSourceHandle, NativeApplicationSource, PropertyMapBuilder,
};
use serde_json::{json, Value};
use std::{future::Future, num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::sync::{mpsc, RwLock};

struct Application {
    core: DrasiLib,
    input: ApplicationSourceHandle,
    source: ComponentHandle,
    output: Endpoint,
}

struct QueryOutput {
    id: String,
    receiver: mpsc::Receiver<InputEnvelope>,
}

impl Application {
    async fn new(scope: &str) -> Result<Self> {
        let core = DrasiLib::builder().with_id(scope).build().await?;
        core.start().await?;
        let control = core.computation_control()?;
        let (source, input) = NativeApplicationSource::channel(
            scope,
            control.desired_snapshot().id.to_string(),
            "input",
            StreamId::try_new("input/out")?,
            NonZeroUsize::new(16).context("capacity")?,
        )?;
        let output = source.output();
        let source = control
            .add_component(
                ComponentAddition::new(ConstructedComponent::source(Box::new(source)))
                    .bind_stream(PortId::try_new("out")?, StreamId::try_new("input/out")?)
                    .defer_activation(),
            )
            .await?;
        Ok(Self {
            core,
            input,
            source,
            output,
        })
    }

    async fn query(
        &self,
        id: &str,
        text: &str,
        bootstrap: Option<Arc<dyn ComputationBootstrapProvider>>,
    ) -> Result<QueryOutput> {
        let control = self.core.computation_control()?;
        let query_id = ComponentId::try_new(id)?;
        let mut pipeline = self
            .core
            .computation_pipeline()?
            .native_source(self.output.clone())?;
        if let Some(bootstrap) = bootstrap {
            pipeline = pipeline.query_bootstrap(id, bootstrap)?;
        }
        let batch = pipeline
            .query(
                drasi_lib::Query::cypher(id)
                    .query(text)
                    .from_source("input")
                    .with_outbox_capacity(16)
                    .build(),
            )
            .build()?;
        let report = self.core.add_components(batch).await?;
        assert!(report.committed, "{report:?}");
        let query = self.core.computation_component(id)?;
        let reaction_id = ComponentId::try_new(format!("{id}/application"))?;
        let (reaction, receiver) = NativeApplicationReaction::channel(
            reaction_id.as_str(),
            QueryChangeCodec::schema().descriptor().clone(),
            NonZeroUsize::new(16).context("reaction capacity")?,
        )?;
        let reaction_input = reaction.input();
        let reaction = control
            .add_component(
                ComponentAddition::new(ConstructedComponent::sink(Box::new(reaction)))
                    .defer_activation(),
            )
            .await?;
        for (from, to) in [(
            Endpoint::new(query_id, PortId::try_new("out")?),
            reaction_input,
        )] {
            let connected = control
                .connect(
                    EdgeDefinition::new(from, to),
                    Box::new(BoundedPipeConfig { capacity: 16 }),
                    RelationshipPolicy::default(),
                )
                .await?;
            assert!(connected.committed, "{connected:?}");
        }
        let mut selected = vec![query.id().clone(), reaction.id().clone()];
        if self.source.observed()?.lifecycle != ComponentLifecycle::Running {
            selected.push(self.source.id().clone());
        }
        let started = control
            .start_components(
                control.desired_snapshot().revision,
                GraphSelection::Exact(selected),
            )
            .await?;
        assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
        query.wait_started().await?;
        reaction.wait_started().await?;
        Ok(QueryOutput {
            id: id.into(),
            receiver,
        })
    }
}

impl QueryOutput {
    async fn row(&mut self, field: &str, value: Value) -> Result<Value> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let input = self
                    .receiver
                    .recv()
                    .await
                    .context("application output closed")?;
                assert_eq!(
                    QueryChangeCodec::metadata(&input.envelope)?.query_id,
                    self.id
                );
                for operation in input.envelope.changes().operations() {
                    let record = match operation {
                        ChangeOperation::Added { after, .. }
                        | ChangeOperation::Updated { after, .. } => after,
                        _ => continue,
                    };
                    let row = QueryChangeCodec::row_values_to_json(
                        &QueryChangeCodec::decode_row(record)?.values,
                    );
                    if row[field] == value {
                        return Ok(row);
                    }
                }
            }
        })
        .await?
    }
}

fn person(id: &str, name: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("input", id),
                labels: Arc::from(vec![Arc::from("Person")]),
                effective_from: 1,
            },
            properties: PropertyMapBuilder::new().with_string("name", name).build(),
        },
    }
}

async fn relationship_direction() -> Result<()> {
    let app = Application::new("native-application-direction").await?;
    let result = async {
        let mut forward = app
            .query(
                "forward",
                "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = 'Alice' RETURN b.name AS friend_name",
                None,
            )
            .await?;
        assert!(app.core.borrow_computation_source("input").await.is_err());
        assert_eq!(app.core.computation_control()?.observed().components.len(), 6);
        assert!(!app.core.computation_control()?.desired_snapshot().specifications.values().any(
            |spec| spec.implementation == SourcePluginAdapterFactory::default().descriptor().implementation
        ));
        for (id, name) in [("alice", "Alice"), ("bob", "Bob")] {
            app.input
                .send_node_insert(
                    id,
                    vec!["Person"],
                    PropertyMapBuilder::new().with_string("name", name).build(),
                )
                .await?;
        }
        app.input
            .send_relation_insert(
                "knows",
                vec!["KNOWS"],
                PropertyMapBuilder::new().build(),
                "alice",
                "bob",
            )
            .await?;
        assert_eq!(
            forward.row("friend_name", json!("Bob")).await?,
            json!({"friend_name": "Bob"})
        );
        app.input
            .send_node_update(
                "bob",
                vec!["Person"],
                PropertyMapBuilder::new().with_string("name", "Robert").build(),
            )
            .await?;
        assert_eq!(
            forward.row("friend_name", json!("Robert")).await?,
            json!({"friend_name": "Robert"})
        );
        let seed = Arc::new(ApplicationBootstrapProvider::with_shared_data(Arc::new(
            RwLock::new(vec![
                person("alice", "Alice"),
                person("bob", "Robert"),
                SourceChange::Insert {
                    element: Element::Relation {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("input", "knows"),
                            labels: Arc::from(vec![Arc::from("KNOWS")]),
                            effective_from: 1,
                        },
                        properties: Default::default(),
                        in_node: ElementReference::new("input", "alice"),
                        out_node: ElementReference::new("input", "bob"),
                    },
                },
            ]),
        )));
        let _reverse = app
            .query(
                "reverse",
                "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = 'Robert' RETURN b.name AS friend_name",
                Some(seed.clone()),
            )
            .await?;
        assert!(app.core.get_query_results("reverse").await?.is_empty());
        let _seeded = app
            .query("seeded", "MATCH (p:Person) RETURN p.name AS name", Some(seed))
            .await?;
        assert_eq!(app.core.get_query_results("seeded").await?.len(), 2);
        assert_eq!(
            app.core.get_query_results("forward").await?,
            [json!({"friend_name": "Robert"})]
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = app.core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

async fn multiple_relationships() -> Result<()> {
    let app = Application::new("native-application-count").await?;
    let result = async {
        let mut output = app.query(
            "friends",
            "MATCH (a:Person {name: 'Alice'})-[:KNOWS]->(friend:Person) RETURN count(friend) AS friend_count, collect(friend.name) AS friend_names",
            None,
        ).await?;
        for (id, name) in [
            ("alice", "Alice"),
            ("bob", "Bob"),
            ("charlie", "Charlie"),
            ("diana", "Diana"),
        ] {
            app.input
                .send_node_insert(
                    id,
                    vec!["Person"],
                    PropertyMapBuilder::new().with_string("name", name).build(),
                )
                .await?;
        }
        for target in ["bob", "charlie", "diana"] {
            app.input.send_relation_insert(
                format!("knows-{target}"),
                vec!["KNOWS"],
                PropertyMapBuilder::new().build(),
                "alice",
                target,
            ).await?;
        }
        let row = output.row("friend_count", json!(3)).await?;
        let mut names = row["friend_names"]
            .as_array()
            .context("friend names")?
            .clone();
        names.sort_by_key(Value::to_string);
        assert_eq!(names, [json!("Bob"), json!("Charlie"), json!("Diana")]);
        assert_eq!(app.core.get_query_results("friends").await?.len(), 1);
        Ok::<_, anyhow::Error>(())
    }.await;
    let cleanup = app.core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

async fn native_query_reconfiguration() -> Result<()> {
    let app = Application::new("native-application-update").await?;
    let result = async {
        let mut output = app
            .query("names", "MATCH (p:Person) RETURN p.name AS name", None)
            .await?;
        let mut observer = app
            .query("observer", "MATCH (p:Person) RETURN p.name AS name", None)
            .await?;
        app.input
            .send_node_insert(
                "one",
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", "Alice")
                    .build(),
            )
            .await?;
        output.row("name", json!("Alice")).await?;
        observer.row("name", json!("Alice")).await?;
        let source_generation = app.source.generation();
        app.core
            .update_query(
                "names",
                drasi_lib::Query::cypher("names")
                    .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
                    .from_source("input")
                    .with_outbox_capacity(16)
                    .build(),
            )
            .await?;
        app.core
            .computation_component("names")?
            .wait_started()
            .await?;
        assert_eq!(app.source.observed()?.generation, source_generation);
        app.input
            .send_node_insert(
                "two",
                vec!["Person"],
                PropertyMapBuilder::new().with_string("name", "Bob").build(),
            )
            .await?;
        assert_eq!(
            output
                .row("name", json!("Bob"))
                .await
                .context("native query did not deliver after replacement")?,
            json!({"name":"Bob", "updated":true})
        );
        observer.row("name", json!("Bob")).await?;
        app.core.stop_query("names").await?;
        assert_eq!(
            app.source.observed()?.lifecycle,
            ComponentLifecycle::Running
        );
        app.input
            .send_node_insert(
                "paused",
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", "While paused")
                    .build(),
            )
            .await?;
        observer.row("name", json!("While paused")).await?;
        app.core.start_query("names").await?;
        app.core
            .computation_component("names")?
            .wait_started()
            .await?;
        output.row("name", json!("While paused")).await?;
        app.input
            .send_node_insert(
                "three",
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", "Charlie")
                    .build(),
            )
            .await?;
        output
            .row("name", json!("Charlie"))
            .await
            .context("native query did not deliver after restart")?;
        observer.row("name", json!("Charlie")).await?;
        app.core.remove_reaction("names/application", false).await?;
        app.core.remove_query("names").await?;
        assert_eq!(app.source.observed()?.generation, source_generation);
        assert_eq!(
            app.source.observed()?.lifecycle,
            ComponentLifecycle::Running
        );
        app.input
            .send_node_insert(
                "after-removal",
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", "Still running")
                    .build(),
            )
            .await?;
        observer.row("name", json!("Still running")).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = app.core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

async fn query_before_native_source() -> Result<()> {
    for (auto_start, explicit_stop, update) in [
        (true, false, false),
        (false, false, false),
        (true, true, false),
        (true, false, true),
        (false, false, true),
        (true, true, true),
    ] {
        late_source_case(auto_start, explicit_stop, update).await?;
    }
    Ok(())
}

async fn late_source_case(auto_start: bool, explicit_stop: bool, update: bool) -> Result<()> {
    let scope = format!("native-late-source-{auto_start}-{explicit_stop}-{update}");
    let core = DrasiLib::builder().with_id(&scope).build().await?;
    let result = async {
        core.start().await?;
        let output = Endpoint::new(ComponentId::try_new("later")?, PortId::try_new("out")?);
        let batch = core
            .computation_pipeline()?
            .native_source(output.clone())?
            .query(
                drasi_lib::Query::cypher("waiting")
                    .query("MATCH (p:Person) RETURN p.name AS name")
                    .from_source("later")
                    .auto_start(auto_start)
                    .build(),
            )
            .build()?;
        let accepted = core.add_components(batch).await?;
        assert!(accepted.committed, "{accepted:?}");
        let mut query = core.computation_component("waiting")?;
        if auto_start {
            assert!(
                query
                    .observed()?
                    .waiting_for
                    .contains(&DependencyWait::Component {
                        id: ComponentId::try_new("later")?,
                    }),
                "{:?}",
                query.observed()?
            );
        }
        if explicit_stop {
            core.stop_query("waiting").await?;
        }
        if update {
            core.update_query(
                "waiting",
                drasi_lib::Query::cypher("waiting")
                    .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
                    .from_source("later")
                    .auto_start(auto_start)
                    .build(),
            )
            .await?;
            query = core.computation_component("waiting")?;
        }
        let (source, input) = NativeApplicationSource::channel(
            scope,
            core.computation_control()?
                .desired_snapshot()
                .id
                .to_string(),
            "later",
            StreamId::try_new("later/out")?,
            NonZeroUsize::new(4).context("capacity")?,
        )?;
        let batch = ComponentBatch::builder()
            .source(Box::new(source))
            .bind_stream(output, StreamId::try_new("later/out")?)
            .build()?;
        assert!(core.add_components(batch).await?.committed);
        if !auto_start || explicit_stop {
            assert_ne!(query.observed()?.lifecycle, ComponentLifecycle::Running);
            core.start_query("waiting").await?;
        }
        tokio::time::timeout(Duration::from_secs(5), query.wait_started())
            .await
            .with_context(|| {
                format!(
                    "late source failed to activate query: {:?}",
                    query.observed()
                )
            })??;
        let mut results = core.subscribe_query_results("waiting").await?.stream;
        input
            .send_node_insert(
                "late",
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", "Ready")
                    .build(),
            )
            .await?;
        use futures::StreamExt;
        let result = tokio::time::timeout(Duration::from_secs(5), results.next())
            .await?
            .context("query reader closed")??;
        let expected = if update {
            json!({"name":"Ready", "updated":true})
        } else {
            json!({"name":"Ready"})
        };
        assert!(matches!(
            &result.results[..],
            [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &expected
        ));
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

async fn bootstrap_survives_query_replacement() -> Result<()> {
    let app = Application::new("native-application-bootstrap-update").await?;
    let result = async {
        let bootstrap = Arc::new(ApplicationBootstrapProvider::with_shared_data(Arc::new(
            RwLock::new(vec![person("seed", "Initial")]),
        )));
        let mut output = app
            .query(
                "seeded",
                "MATCH (p:Person) RETURN p.name AS name",
                Some(bootstrap),
            )
            .await?;
        assert_eq!(
            app.core.get_query_results("seeded").await?,
            [json!({"name":"Initial"})]
        );
        app.core
            .update_query(
                "seeded",
                drasi_lib::Query::cypher("seeded")
                    .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
                    .from_source("input")
                    .with_outbox_capacity(16)
                    .build(),
            )
            .await?;
        app.core
            .computation_component("seeded")?
            .wait_started()
            .await?;
        assert_eq!(
            app.core.get_query_results("seeded").await?,
            [json!({"name":"Initial","updated":true})]
        );
        assert!(
            output.receiver.try_recv().is_err(),
            "bootstrap must not fabricate live changes"
        );
        app.input
            .send_node_insert(
                "live",
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", "Live")
                    .build(),
            )
            .await?;
        assert_eq!(
            output.row("name", json!("Live")).await?,
            json!({"name":"Live","updated":true})
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = app.core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

async fn custom_inputs_are_not_replaced() -> Result<()> {
    let app = Application::new("native-application-custom-input").await?;
    let result = async {
        let mut output = app
            .query("names", "MATCH (p:Person) RETURN p.name AS name", None)
            .await?;
        let control = app.core.computation_control()?;
        let generation = app.core.computation_component("names")?.generation();
        let edge = EdgeDefinition::new(
            app.output.clone(),
            Endpoint::new(ComponentId::try_new("names")?, PortId::try_new("in")?),
        );
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::Bind(DesiredRelationship {
                    definition: edge.clone(),
                    pipe: DesiredPipe::Bounded { capacity: 16 },
                    policy: RelationshipPolicy::default(),
                })],
            )
            .await?;
        assert!(
            control
                .reconcile(preview, TopologyBindings::default())
                .await?
                .committed
        );
        let error = app
            .core
            .update_query(
                "names",
                drasi_lib::Query::cypher("names")
                    .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
                    .from_source("input")
                    .build(),
            )
            .await
            .expect_err("custom graph inputs cannot be replaced through QueryConfig");
        assert!(
            error
                .to_string()
                .contains("computation topology operations"),
            "{error}"
        );
        assert_eq!(
            app.core.computation_component("names")?.generation(),
            generation
        );
        assert!(control.desired_snapshot().edges.iter().any(|current| {
            current.definition == edge && current.pipe == DesiredPipe::Bounded { capacity: 16 }
        }));
        app.input
            .send_node_insert(
                "one",
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", "Unchanged")
                    .build(),
            )
            .await?;
        assert_eq!(
            output.row("name", json!("Unchanged")).await?,
            json!({"name":"Unchanged"})
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = app.core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

async fn native_pipeline_validates_configuration() -> Result<()> {
    let core = DrasiLib::builder()
        .with_id("native-input-validation")
        .build()
        .await?;
    let result = async {
        let output = Endpoint::new(ComponentId::try_new("input")?, PortId::try_new("out")?);
        let bootstrap = Arc::new(ApplicationBootstrapProvider::with_shared_data(Arc::new(
            RwLock::new(Vec::new()),
        )));
        for (bootstrap_id, enabled, source, expected) in [
            ("unknown", true, "input", "bootstrap names unknown query"),
            ("names", false, "input", "requires enabled bootstrap"),
            ("names", true, "legacy", "native input bindings"),
        ] {
            let error = core
                .computation_pipeline()?
                .native_source(output.clone())?
                .query_bootstrap(bootstrap_id, bootstrap.clone())?
                .query(
                    drasi_lib::Query::cypher("names")
                        .query("MATCH (p:Person) RETURN p.name AS name")
                        .from_source(source)
                        .enable_bootstrap(enabled)
                        .build(),
                )
                .build()
                .err()
                .context("invalid bootstrap declaration was accepted")?;
            assert!(error.to_string().contains(expected), "{error}");
        }
        let mut query = drasi_lib::Query::cypher("names")
            .query("MATCH (p:Person) RETURN p.name AS name")
            .from_source("input")
            .build();
        query.sources[0].nodes.push("Person".into());
        let error = core
            .computation_pipeline()?
            .native_source(output.clone())?
            .query(query)
            .build()
            .err()
            .context("Source-plugin selectors must not be ignored")?;
        assert!(error.to_string().contains("label selection"), "{error}");
        let mut batch = core
            .computation_pipeline()?
            .native_source(output)?
            .query(
                drasi_lib::Query::cypher("names")
                    .query("MATCH (p:Person) RETURN p.name AS name")
                    .from_source("input")
                    .build(),
            )
            .build()?;
        batch.definition.boundary_relationships[0].pipe = DesiredPipe::Bounded { capacity: 0 };
        assert!(
            core.add_components(batch).await.is_err(),
            "missing sources cannot hide invalid pipes"
        );
        assert!(core.computation_component("names").is_err());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

fn both_runtimes<F, Fut>(case: F) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    for current_thread in [true, false] {
        let mut builder = if current_thread {
            tokio::runtime::Builder::new_current_thread()
        } else {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        };
        builder
            .enable_all()
            .build()?
            .block_on(async { tokio::time::timeout(Duration::from_secs(20), case()).await? })?;
    }
    Ok(())
}

#[test]
fn native_application_relationship_direction_and_bootstrap_work_inside_drasi_lib() -> Result<()> {
    both_runtimes(relationship_direction)
}

#[test]
fn native_application_relationship_aggregation_matches_existing_application_behavior() -> Result<()>
{
    both_runtimes(multiple_relationships)
}

#[test]
fn native_application_query_updates_and_stop_preserve_the_source_owner() -> Result<()> {
    both_runtimes(native_query_reconfiguration)
}

#[test]
fn native_application_query_declared_before_its_source_activates_without_readdition() -> Result<()>
{
    both_runtimes(query_before_native_source)
}

#[test]
fn native_application_query_replacement_preserves_bootstrap_and_live_delivery() -> Result<()> {
    both_runtimes(bootstrap_survives_query_replacement)
}

#[test]
fn native_application_query_replacement_rejects_custom_inputs_without_mutation() -> Result<()> {
    both_runtimes(custom_inputs_are_not_replaced)
}

#[test]
fn native_application_pipeline_rejects_invalid_bootstrap_selectors_and_pipes() -> Result<()> {
    both_runtimes(native_pipeline_validates_configuration)
}
