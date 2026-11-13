// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_bootstrap_application::ApplicationBootstrapProvider;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::{computation::v1::*, DrasiLib};
use drasi_reaction_application::NativeApplicationReaction;
use drasi_source_application::{
    ApplicationSourceHandle, NativeApplicationSource, PropertyMapBuilder,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify, RwLock};

struct GatedBootstrap {
    inner: ApplicationBootstrapProvider,
    hold: AtomicBool,
    fail: AtomicBool,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl ComputationBootstrapProvider for GatedBootstrap {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        if self.hold.load(Ordering::Acquire) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        anyhow::ensure!(
            !self.fail.load(Ordering::Acquire),
            "deliberate replacement bootstrap failure"
        );
        self.inner.snapshot().await
    }
}

struct Application {
    core: DrasiLib,
    input: ApplicationSourceHandle,
    source: ComponentHandle,
    output: Endpoint,
}

struct QueryOutput {
    id: ComponentId,
    receiver: mpsc::Receiver<InputEnvelope>,
}

fn component_id(value: u128) -> ComponentId {
    ComponentId::try_new(format!("{value:032x}")).expect("numeric fixture UUID")
}

fn component_name(name: &str) -> Result<ComponentName> {
    Ok(ComponentName::try_new(name)?)
}

impl Application {
    async fn new(scope: &str) -> Result<Self> {
        let core = lib_integration_tests::query_builder()
            .with_id(scope)
            .build()
            .await?;
        core.start().await?;
        let control = core.computation_control()?;
        let source_id = component_id(113);
        let (source, input) = NativeApplicationSource::channel(
            scope,
            control.desired_snapshot().id.to_string(),
            source_id.clone(),
            StreamId::try_new("input/out")?,
            NonZeroUsize::new(16).context("capacity")?,
        )?;
        let output = source.output();
        let mut observed = control.subscribe_observed();
        core.add_components(
            ComponentBatch::builder()
                .source(component_name("input")?, Box::new(source))
                .bind_stream(output.clone(), StreamId::try_new("input/out")?)
                .build()?
                .auto_start(false),
        )
        .await?;
        observed
            .wait_for(|graph| graph.components.contains_key(&source_id))
            .await?;
        let source = core.component_by_id(&source_id)?;
        Ok(Self {
            core,
            input,
            source,
            output,
        })
    }

    async fn query(
        &self,
        id: ComponentId,
        name: ComponentName,
        text: &str,
        bootstrap: Option<Arc<dyn ComputationBootstrapProvider>>,
    ) -> Result<QueryOutput> {
        self.query_with_provider(id, name, text, bootstrap, None, None)
            .await
    }

    async fn query_with_provider(
        &self,
        query_id: ComponentId,
        name: ComponentName,
        text: &str,
        bootstrap: Option<Arc<dyn ComputationBootstrapProvider>>,
        indexes: Option<Arc<dyn drasi_core::computation::ComputationIndexProvider>>,
        input_pipe: Option<DesiredPipe>,
    ) -> Result<QueryOutput> {
        let control = self.core.computation_control()?;
        let mut pipeline = self
            .core
            .computation_pipeline()?
            .native_source(self.output.clone())?;
        let enable_bootstrap = bootstrap.is_some();
        if let Some(bootstrap) = bootstrap {
            pipeline = pipeline.query_bootstrap(&query_id, bootstrap)?;
        }
        if let Some(indexes) = indexes {
            pipeline =
                pipeline.query_provider(&query_id, indexes, QueryPublicationMode::NonAtomic)?;
        }
        let mut batch = pipeline
            .query(
                drasi_lib::Query::cypher(name.clone())
                    .query(text)
                    .from_input_port_by_id(self.output.component.clone(), self.output.port.clone())
                    .auto_start(false)
                    .enable_bootstrap(enable_bootstrap)
                    .with_outbox_capacity(16)
                    .build()?
                    .with_assigned_id(query_id.clone()),
            )
            .build()?;
        batch
            .definition
            .relationships
            .append(&mut batch.definition.boundary_relationships);
        if let Some(pipe) = input_pipe {
            batch
                .definition
                .relationships
                .iter_mut()
                .find(|edge| {
                    edge.definition.from == self.output && edge.definition.to.component == query_id
                })
                .context("native query input relationship")?
                .pipe = pipe;
        }
        let query_members: Vec<_> = batch
            .definition
            .components
            .iter()
            .map(|component| component.descriptor.id().clone())
            .collect();
        let mut observed = control.subscribe_observed();
        self.core.add_components(batch).await?;
        observed
            .wait_for(|graph| {
                query_members
                    .iter()
                    .all(|id| graph.components.contains_key(id))
            })
            .await?;
        let query = self.core.component_by_id(&query_id)?;
        let reaction_name = ComponentName::try_new(format!("{name}-application"))?;
        let (reaction, receiver) = NativeApplicationReaction::channel(
            ComponentId::generate(),
            QueryChangeCodec::schema().descriptor().clone(),
            NonZeroUsize::new(16).context("reaction capacity")?,
        )?;
        let reaction_input = reaction.input();
        let mut batch = ComponentBatch::builder()
            .sink(reaction_name, Box::new(reaction))
            .build()?
            .auto_start(false);
        batch.definition.relationships.push(DesiredRelationship {
            definition: EdgeDefinition::new(
                Endpoint::new(query_id.clone(), PortId::try_new("out")?),
                reaction_input.clone(),
            ),
            pipe: DesiredPipe::Bounded { capacity: 16 },
            policy: RelationshipPolicy::default(),
        });
        self.core.add_components(batch).await?;
        observed
            .wait_for(|graph| graph.components.contains_key(&reaction_input.component))
            .await?;
        let reaction = self.core.component_by_id(&reaction_input.component)?;
        self.source.wait_created().await.with_context(|| {
            format!(
                "source {} creation after complete query wiring: {:?}",
                self.source.id(),
                self.source.observed()
            )
        })?;
        for id in &query_members {
            let member = self.core.component_by_id(id)?;
            member.wait_created().await.with_context(|| {
                format!(
                    "query member {id} creation after complete reaction wiring: {:?}",
                    member.observed()
                )
            })?;
        }
        reaction.wait_created().await.with_context(|| {
            format!(
                "reaction {} creation after complete query wiring: {:?}",
                reaction.id(),
                reaction.observed()
            )
        })?;
        let mut selected = query_members;
        selected.push(reaction.id().clone());
        if self.source.observed()?.lifecycle != ComponentLifecycle::Running {
            selected.push(self.source.id().clone());
        }
        let members = selected
            .iter()
            .map(|id| self.core.component_by_id(id))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let started = futures::future::join_all(members.iter().map(|member| member.start())).await;
        for (member, result) in members.iter().zip(started) {
            result.with_context(|| {
                format!(
                    "queued start for {} generation {:?}; observed state: {:?}",
                    member.id(),
                    member.generation(),
                    member.observed()
                )
            })?;
        }
        query.wait_started().await?;
        reaction.wait_started().await?;
        Ok(QueryOutput {
            id: query_id,
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
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000045d").expect("fixture UUID"), drasi_lib::ComponentName::try_new("forward").expect("fixture component name"),
                "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = 'Alice' RETURN b.name AS friend_name",
                None,
            )
            .await?;
        assert!(app.core.borrow_computation_source_by_id(&drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000071").expect("fixture UUID")).await.is_err());
        let expected = BTreeSet::from([
            app.source.id().clone(),
            forward.id.clone(),
            ComponentId::derive_owned(&forward.id, "query-scheduled")?,
            ComponentId::derive_owned(&forward.id, "query-results")?,
            app.core
                .component_by_name(&component_name("forward-application")?)?
                .id()
                .clone(),
        ]);
        assert_eq!(
            app.core
                .computation_control()?
                .observed()
                .components
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected
        );
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
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000045e").expect("fixture UUID"), drasi_lib::ComponentName::try_new("reverse").expect("fixture component name"),
                "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = 'Robert' RETURN b.name AS friend_name",
                Some(seed.clone()),
            )
            .await?;
        assert!(app.core.get_query_results_by_name(&drasi_core::ComponentName::try_new("reverse").expect("fixture component name")).await?.is_empty());
        let _seeded = app
            .query(drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000045f").expect("fixture UUID"), drasi_lib::ComponentName::try_new("seeded").expect("fixture component name"), "MATCH (p:Person) RETURN p.name AS name", Some(seed))
            .await?;
        assert_eq!(app.core.get_query_results_by_name(&drasi_core::ComponentName::try_new("seeded").expect("fixture component name")).await?.len(), 2);
        assert_eq!(
            app.core.get_query_results_by_name(&drasi_core::ComponentName::try_new("forward").expect("fixture component name")).await?,
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
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000460").expect("fixture UUID"), drasi_lib::ComponentName::try_new("friends").expect("fixture component name"),
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
        assert_eq!(app.core.get_query_results_by_name(&drasi_core::ComponentName::try_new("friends").expect("fixture component name")).await?.len(), 1);
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
            .query(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000030e")
                    .expect("fixture UUID"),
                drasi_lib::ComponentName::try_new("names").expect("fixture component name"),
                "MATCH (p:Person) RETURN p.name AS name",
                None,
            )
            .await?;
        let mut observer = app
            .query(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001b5")
                    .expect("fixture UUID"),
                drasi_lib::ComponentName::try_new("observer").expect("fixture component name"),
                "MATCH (p:Person) RETURN p.name AS name",
                None,
            )
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
        let names = app.core.component_by_id(&output.id)?;
        let generation = names.generation();
        let unsupported = drasi_lib::Query::cypher(component_name("names")?)
            .query("MATCH (p:Person) RETURN p.name AS name")
            .from_input_by_id(component_id(778))
            .enable_bootstrap(false)
            .with_outbox_capacity(16)
            .build()?;
        let error = app.core.update_query_by_name(&component_name("names")?, unsupported)
            .await.expect_err("live input rebinding must be rejected before replacement");
        assert!(error.to_string().contains("cannot change live inputs"), "{error}");
        assert_eq!(names.observed()?.generation, generation);
        assert_eq!(app.core.get_query_results_by_name(&component_name("names")?).await?,
            [json!({"name":"Alice"})]);
        app.core
            .update_query_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
                drasi_lib::Query::cypher(
                    drasi_core::ComponentName::try_new("names").expect("fixture component name"),
                )
                .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
                .from_input_port_by_id(app.output.component.clone(), app.output.port.clone())
                .enable_bootstrap(false)
                .with_outbox_capacity(16)
                .build()?,
            )
            .await?;
        let resolved = app.core
            .component_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )?;
        tokio::time::timeout(Duration::from_secs(10), resolved.wait_started())
            .await
            .with_context(|| format!("resolved query did not become ready: {:?}; pending: {:?}",
                resolved.observed(), app.core.desired_configuration()))??;
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
        app.core
            .stop_query_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )
            .await?;
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
        app.core
            .start_query_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )
            .await?;
        app.core
            .component_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )?
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
        app.core.stop_query_by_name(&component_name("names")?).await?;
        let observer_generation = app.core.component_by_id(&observer.id)?.generation();
        app.core.update_query_by_name(&component_name("names")?,
            drasi_lib::Query::cypher(component_name("names")?)
                .query("MATCH (p:Person) WHERE p.name <> 'Bob' RETURN p.name AS name, true AS updated")
                .from_input_port_by_id(app.output.component.clone(), app.output.port.clone())
                .enable_bootstrap(false)
                .with_outbox_capacity(16)
                .build()?,
        ).await?;
        assert_eq!(app.core.component_by_id(&output.id)?.observed()?.lifecycle,
            ComponentLifecycle::Stopped);
        assert_eq!(app.source.observed()?.generation, source_generation);
        assert_eq!(app.core.component_by_id(&observer.id)?.observed()?.generation,
            observer_generation);
        assert_eq!(app.core.get_query_results_by_name(&component_name("observer")?).await?.len(), 4);
        app.core.start_query_by_name(&component_name("names")?).await?;
        app.core.component_by_id(&output.id)?.wait_started().await?;
        assert!(app.core.get_query_results_by_name(&component_name("names")?).await?.is_empty(),
            "streaming-only semantic replacement must clear incompatible rows");
        app.input.send_node_insert("four", vec!["Person"],
            PropertyMapBuilder::new().with_string("name", "David").build()).await?;
        assert_eq!(output.row("name", json!("David")).await?,
            json!({"name":"David","updated":true}));
        observer.row("name", json!("David")).await?;
        app.core
            .remove_reaction_by_name(
                &drasi_core::ComponentName::try_new("names-application")
                    .expect("fixture component name"),
                false,
            )
            .await?;
        app.core
            .remove_query_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )
            .await?;
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
        late_source_case(
            auto_start,
            explicit_stop,
            update,
            drasi_core::ComponentName::try_new("later").expect("fixture component name"),
            component_id(778),
        )
        .await?;
        if update {
            late_source_case(
                auto_start,
                explicit_stop,
                update,
                drasi_core::ComponentName::try_new("superseded").expect("fixture component name"),
                component_id(779),
            )
            .await?;
        }
    }
    Ok(())
}

async fn late_source_case(
    auto_start: bool,
    explicit_stop: bool,
    update: bool,
    initial_source: drasi_core::ComponentName,
    initial_source_id: ComponentId,
) -> Result<()> {
    let scope =
        format!("native-late-source-{auto_start}-{explicit_stop}-{update}-{initial_source}");
    let core = lib_integration_tests::query_builder()
        .with_id(&scope)
        .build()
        .await?;
    let result = async {
        core.start().await?;
        let output = Endpoint::new(initial_source_id.clone(), PortId::try_new("out")?);
        let mut batch = core
            .computation_pipeline()?
            .native_source(output.clone())?
            .query(
                drasi_lib::Query::cypher(
                    drasi_core::ComponentName::try_new("waiting").expect("fixture component name"),
                )
                .query("MATCH (p:Person) RETURN p.name AS name")
                .from_input_port_by_id(initial_source_id.clone(), PortId::try_new("out")?)
                .auto_start(auto_start)
                .enable_bootstrap(false)
                .build()?
                .with_assigned_id(component_id(781)),
            )
            .build()?;
        batch
            .definition
            .relationships
            .append(&mut batch.definition.boundary_relationships);
        let mut observed = core.computation_control()?.inspector().subscribe();
        core.add_components(batch).await?;
        observed.wait_for(|graph| graph.desired.component_by_name(
            &component_name("waiting").expect("fixture component name")).is_some()).await?;
        let mut query = core.component_by_name(
            &drasi_core::ComponentName::try_new("waiting").expect("fixture component name"),
        ).with_context(|| format!("lookup initial waiting query ({auto_start}, {explicit_stop}, {update}, {initial_source})"))?;
        if auto_start {
            observed.wait_for(|graph| graph.observed.components.get(&component_id(781))
                .is_some_and(|node| node.waiting_for.contains(&DependencyWait::Component {
                    id: initial_source_id.clone(),
                }))).await?;
            assert!(
                query
                    .observed()?
                    .waiting_for
                    .contains(&DependencyWait::Component {
                        id: initial_source_id,
                    }),
                "{:?}",
                query.observed()?
            );
        }
        if explicit_stop {
            core.stop_query_by_name(
                &drasi_core::ComponentName::try_new("waiting").expect("fixture component name"),
            )
            .await.context("stop initial waiting query")?;
        }
        if update {
            let mut replacement = drasi_lib::Query::cypher(
                drasi_core::ComponentName::try_new("waiting").expect("fixture component name"),
            )
            .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
            .from_input_by_id(component_id(778))
            .auto_start(auto_start)
            .enable_bootstrap(false)
            .build()?;
            if initial_source
                != drasi_core::ComponentName::try_new("later").expect("fixture component name")
            {
                replacement.inputs[0].port = Some(
                    drasi_lib::computation::v1::PortId::try_new("out").expect("fixture PortId"),
                );
            }
            core.update_query_by_name(
                &drasi_core::ComponentName::try_new("waiting").expect("fixture component name"),
                replacement,
            )
            .await.context("update waiting query")?;
            query = core.component_by_name(
                &drasi_core::ComponentName::try_new("waiting").expect("fixture component name"),
            ).context("lookup updated waiting query")?;
            let observed = query.observed()?;
            assert!(observed.failure.is_none(), "{observed:?}");
            assert_ne!(observed.lifecycle, ComponentLifecycle::Running);
            if auto_start && !explicit_stop {
                assert!(
                    observed.waiting_for.contains(&DependencyWait::Component {
                        id: component_id(778),
                    }),
                    "{observed:?}"
                );
            }
        }
        let later_id = component_id(778);
        let (source, input) = NativeApplicationSource::channel(
            scope,
            core.computation_control()?
                .desired_snapshot()
                .id
                .to_string(),
            later_id.clone(),
            StreamId::try_new("later/out")?,
            NonZeroUsize::new(4).context("capacity")?,
        )?;
        let batch = ComponentBatch::builder()
            .source(component_name("later")?, Box::new(source))
            .bind_stream(
                Endpoint::new(later_id, PortId::try_new("out")?),
                StreamId::try_new("later/out")?,
            )
            .build()?;
        core.add_components(batch).await?;
        if !auto_start || explicit_stop {
            assert_ne!(query.observed()?.lifecycle, ComponentLifecycle::Running);
            core.start_query_by_name(
                &drasi_core::ComponentName::try_new("waiting").expect("fixture component name"),
            )
            .await.context("start resolved waiting query")?;
        }
        tokio::time::timeout(Duration::from_secs(5), query.wait_started())
            .await
            .with_context(|| {
                format!(
                    "late source failed to activate query: {:?}",
                    query.observed()
                )
            })??;
        let mut results = core
            .subscribe_query_results_by_name(
                &drasi_core::ComponentName::try_new("waiting").expect("fixture component name"),
            )
            .await.with_context(|| format!("late source subscription ({auto_start}, {explicit_stop}, {update}, {initial_source}): {:?}", query.observed()))?
            .stream;
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

struct CountingIndexes(AtomicUsize);

#[async_trait]
impl drasi_core::computation::ComputationIndexProvider for CountingIndexes {
    async fn create_indexes(
        &self,
        graph: &str,
        query: &drasi_core::ComponentId,
    ) -> std::result::Result<
        drasi_core::computation::ComputationIndexes,
        drasi_core::interface::IndexError,
    > {
        self.0.fetch_add(1, Ordering::SeqCst);
        drasi_core::computation::InMemoryComputationProvider
            .create_indexes(graph, query)
            .await
    }

    fn is_volatile(&self) -> bool {
        true
    }
}

struct PrimaryApplicationSource {
    descriptor: ComponentDescriptor,
    inner: NativeApplicationSource,
}

#[async_trait]
impl ComputationComponent for PrimaryApplicationSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> Result<()> {
        self.inner.start().await
    }

    async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await
    }
}

#[async_trait]
impl EnvelopeSource for PrimaryApplicationSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.inner.next().await?.map(|mut output| {
            output.port = PortId::try_new("data").expect("primary port");
            output
        }))
    }
}

struct PrimaryApplicationFactory {
    descriptor: FactoryDescriptor,
    source: Mutex<Option<NativeApplicationSource>>,
    created: AtomicUsize,
}

#[async_trait]
impl ComponentFactory for PrimaryApplicationFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn primary_ports(&self) -> PortPrimaries {
        PortPrimaries::new(None, Some(PortId::try_new("data").expect("primary port")))
    }

    fn validate(&self, _: &ComponentSpecification) -> Result<()> {
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let source = self
            .source
            .lock()
            .expect("fixture source")
            .take()
            .context("query replacement must not reconstruct its producer")
            .map_err(ComponentCreationError::terminal)?;
        self.created.fetch_add(1, Ordering::SeqCst);
        Ok(ConstructedComponent::source(Box::new(
            PrimaryApplicationSource {
                descriptor: context.specification.descriptor.clone(),
                inner: source,
            },
        )))
    }
}

async fn deferred_native_primary_preserves_resources() -> Result<()> {
    let app = Application::new("native-deferred-primary").await?;
    let result = async {
        let mut observer = app
            .query(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001b5")
                    .expect("fixture UUID"),
                drasi_lib::ComponentName::try_new("observer").expect("fixture component name"),
                "MATCH (p:Person) RETURN p.name AS name",
                None,
            )
            .await?;
        let indexes = Arc::new(CountingIndexes(AtomicUsize::new(0)));
        let bootstrap = Arc::new(ApplicationBootstrapProvider::with_shared_data(Arc::new(
            RwLock::new(vec![person("seed", "Initial")]),
        )));
        let control = app.core.computation_control()?;
        let later_id = component_id(778);
        let names_id = component_id(782);
        let batch = app
            .core
            .computation_pipeline()?
            .native_source(Endpoint::new(later_id.clone(), PortId::try_new("data")?))?
            .query_bootstrap(&names_id, bootstrap)?
            .query_provider(&names_id, indexes.clone(), QueryPublicationMode::NonAtomic)?
            .query(
                drasi_lib::Query::cypher(component_name("names")?)
                    .query("MATCH (p:Person) RETURN p.name AS name")
                    .from_input_port_by_id(later_id.clone(), PortId::try_new("data")?)
                    .enable_bootstrap(true)
                    .with_outbox_capacity(16)
                    .build()?
                    .with_assigned_id(names_id.clone()),
            )
            .build()?;
        app.core.add_components(batch).await?;
        let (reaction, receiver) = NativeApplicationReaction::channel(
            ComponentId::generate(),
            QueryChangeCodec::schema().descriptor().clone(),
            NonZeroUsize::new(16).context("reaction capacity")?,
        )?;
        let reaction_input = reaction.input();
        app.core
            .add_components(
                ComponentBatch::builder()
                    .sink(component_name("names-application")?, Box::new(reaction))
                    .connect(
                        EdgeDefinition::new(
                            Endpoint::new(names_id.clone(), PortId::try_new("out")?),
                            reaction_input,
                        ),
                        Box::new(BoundedPipeConfig { capacity: 16 }),
                    )
                    .build()?,
            )
            .await?;
        let mut output = QueryOutput {
            id: names_id.clone(),
            receiver,
        };
        let original_source_generation = app.source.generation();
        let replacement = drasi_lib::Query::cypher(
            drasi_core::ComponentName::try_new("names").expect("fixture component name"),
        )
        .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
        .from_input_port_by_id(component_id(778), PortId::try_new("data")?)
        .enable_bootstrap(true)
        .with_outbox_capacity(16)
        .build()?;
        app.core
            .update_query_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
                replacement.clone(),
            )
            .await?;
        let pending = app
            .core
            .component_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )?
            .observed()?;
        assert!(pending.failure.is_none(), "{pending:?}");
        assert_ne!(pending.lifecycle, ComponentLifecycle::Running);
        assert!(!pending.waiting_for.is_empty(), "{pending:?}");
        assert!(control
            .desired_snapshot()
            .edges
            .iter()
            .all(|edge| edge.definition.from.component != later_id));
        assert!(
            control
                .desired_snapshot()
                .unbound_relationships
                .iter()
                .any(|edge| edge.definition.to.component == names_id),
            "the absent input must remain an explicit dependency"
        );
        // A second update must recognize the internal boundary by logical input.
        app.core
            .update_query_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
                replacement,
            )
            .await?;
        let before_resume = indexes.0.load(Ordering::SeqCst);
        assert_eq!(
            before_resume, 0,
            "waiting declarations must not open processing state"
        );
        let (source, input) = NativeApplicationSource::channel(
            "native-deferred-primary",
            control.desired_snapshot().id.to_string(),
            later_id.clone(),
            StreamId::try_new("later/data")?,
            NonZeroUsize::new(4).context("capacity")?,
        )?;
        let factory = Arc::new(PrimaryApplicationFactory {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/primary-application", "1")?,
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::new(),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            source: Mutex::new(Some(source)),
            created: AtomicUsize::new(0),
        });
        let descriptor = ComponentDescriptor::try_new(
            later_id.clone(),
            ["auxiliary", "data"]
                .into_iter()
                .map(|port| {
                    Ok(PortDescriptor::new(
                        PortId::try_new(port)?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        )?;
        let unused_id = component_id(783);
        let (sink, _unused) = NativeApplicationReaction::channel(
            unused_id.clone(),
            GraphChangeCodec::schema().descriptor().clone(),
            NonZeroUsize::new(4).context("capacity")?,
        )?;
        let batch = ComponentBatch::builder()
            .component(
                component_name("later")?,
                ComponentSpecification {
                    descriptor,
                    role: ComponentRole::Source,
                    completion: None,
                    implementation: factory.descriptor.implementation.clone(),
                    configuration_version: 1,
                    configuration: BTreeMap::new(),
                    dependencies: BTreeMap::new(),
                },
                factory.clone(),
            )
            .sink(component_name("unused")?, Box::new(sink))
            .bind_stream(
                Endpoint::new(later_id.clone(), PortId::try_new("data")?),
                StreamId::try_new("later/data")?,
            )
            .bind_stream(
                Endpoint::new(later_id.clone(), PortId::try_new("auxiliary")?),
                StreamId::try_new("later/auxiliary")?,
            )
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(later_id.clone(), PortId::try_new("auxiliary")?),
                    Endpoint::new(unused_id, PortId::try_new("in")?),
                ),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            )
            .build()?;
        app.core.add_components(batch).await?;
        app.core
            .component_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )?
            .wait_started()
            .await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match app
                    .core
                    .get_query_status_by_name(
                        &drasi_core::ComponentName::try_new("names")
                            .expect("fixture component name"),
                    )
                    .await?
                {
                    drasi_lib::channels::ComponentStatus::Running => {
                        return Ok::<_, anyhow::Error>(())
                    }
                    drasi_lib::channels::ComponentStatus::Error => {
                        anyhow::bail!("resolved query failed: {:?}", app.core.get_graph().await)
                    }
                    _ => tokio::task::yield_now().await,
                }
            }
        })
        .await??;
        assert!(indexes.0.load(Ordering::SeqCst) > before_resume);
        assert_eq!(
            app.core
                .get_query_results_by_name(
                    &drasi_core::ComponentName::try_new("names").expect("fixture component name")
                )
                .await?,
            [json!({"name":"Initial","updated":true})],
            "the explicit native bootstrap and catalogue must survive deferred assembly"
        );
        let desired = control.desired_snapshot();
        let inputs: Vec<_> = desired
            .edges
            .iter()
            .filter(|edge| {
                edge.definition.from.component == later_id
                    && edge.definition.to.component == names_id
            })
            .collect();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].definition.from.port.as_str(), "data");
        assert!(desired
            .unbound_relationships
            .iter()
            .all(|edge| edge.definition.to.component != names_id));
        let producer = app.core.component_by_name(
            &drasi_core::ComponentName::try_new("later").expect("fixture component name"),
        )?;
        let generation = producer.generation();
        input
            .send_node_insert(
                "live",
                vec!["Person"],
                PropertyMapBuilder::new()
                    .with_string("name", "Ready")
                    .build(),
            )
            .await?;
        assert_eq!(
            output.row("name", json!("Ready")).await?,
            json!({"name":"Ready","updated":true})
        );
        app.core
            .stop_query_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )
            .await?;
        assert_eq!(producer.observed()?.generation, generation);
        assert_eq!(producer.observed()?.lifecycle, ComponentLifecycle::Running);
        assert_eq!(factory.created.load(Ordering::SeqCst), 1);
        assert_eq!(
            app.source.observed()?.generation,
            original_source_generation
        );
        assert_eq!(
            app.source.observed()?.lifecycle,
            ComponentLifecycle::Running
        );
        app.input
            .send_node_insert(
                "original",
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

async fn bootstrap_survives_query_replacement() -> Result<()> {
    let app = Application::new("native-application-bootstrap-update").await?;
    let result = async {
        let bootstrap = Arc::new(GatedBootstrap {
            inner: ApplicationBootstrapProvider::with_shared_data(Arc::new(
                RwLock::new(vec![person("seed", "Initial")]))),
            hold: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let mut output = app
            .query(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000045f")
                    .expect("fixture UUID"),
                drasi_lib::ComponentName::try_new("seeded").expect("fixture component name"),
                "MATCH (p:Person) RETURN p.name AS name",
                Some(bootstrap.clone()),
            )
            .await?;
        assert_eq!(
            app.core
                .get_query_results_by_name(
                    &drasi_core::ComponentName::try_new("seeded").expect("fixture component name")
                )
                .await?,
            [json!({"name":"Initial"})]
        );
        bootstrap.hold.store(true, Ordering::Release);
        let name = component_name("seeded")?;
        let update = app.core
            .update_query_by_name(
                &name,
                drasi_lib::Query::cypher(
                    drasi_core::ComponentName::try_new("seeded").expect("fixture component name"),
                )
                .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
                .from_input_port_by_id(app.output.component.clone(), app.output.port.clone())
                .with_outbox_capacity(16)
                .build()?,
            );
        tokio::pin!(update);
        tokio::select! {
            result = &mut update => anyhow::bail!("update completed before held bootstrap: {result:?}"),
            _ = bootstrap.entered.notified() => {}
        }
        assert!(!app.core.desired_configuration()?.pending_updates.is_empty());
        assert!(output.receiver.try_recv().is_err(),
            "live processing must not run while bootstrap is incomplete");
        let source_generation = app.source.generation();
        app.input.send_node_insert("during", vec!["Person"],
            PropertyMapBuilder::new().with_string("name", "During bootstrap").build()).await?;
        let SourceChange::Insert { element } = person("seed", "Revised bootstrap") else {
            unreachable!("person fixture is an insertion");
        };
        let SourceChange::Insert { element: temporary } = person("temporary", "Temporary") else {
            unreachable!("person fixture is an insertion");
        };
        let metadata = temporary.get_metadata().clone();
        app.input.send_batch(vec![
            SourceChange::update(element),
            SourceChange::Insert { element: temporary },
            SourceChange::Delete { metadata },
        ]).await?;
        assert_eq!(app.source.observed()?.lifecycle, ComponentLifecycle::Running);
        bootstrap.hold.store(false, Ordering::Release);
        bootstrap.release.notify_one();
        update.await?;
        app.core
            .component_by_name(
                &drasi_core::ComponentName::try_new("seeded").expect("fixture component name"),
            )?
            .wait_started()
            .await?;
        assert_eq!(output.row("name", json!("During bootstrap")).await?,
            json!({"name":"During bootstrap","updated":true}));
        assert_eq!(output.row("name", json!("Revised bootstrap")).await?,
            json!({"name":"Revised bootstrap","updated":true}));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let input = output.receiver.recv().await.context("application output closed")?;
                for operation in input.envelope.changes().operations() {
                    if let ChangeOperation::Deleted { before: Some(before), .. } = operation {
                        let row = QueryChangeCodec::row_values_to_json(
                            &QueryChangeCodec::decode_row(before)?.values);
                        if row["name"] == "Temporary" {
                            return Ok::<_, anyhow::Error>(());
                        }
                    }
                }
            }
        }).await??;
        let mut rows = app.core.get_query_results_by_name(&component_name("seeded")?).await?;
        rows.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
        assert_eq!(rows, [
            json!({"name":"During bootstrap","updated":true}),
            json!({"name":"Revised bootstrap","updated":true}),
        ]);
        assert_eq!(app.source.observed()?.generation, source_generation);
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
        bootstrap.fail.store(true, Ordering::Release);
        let error = app.core.update_query_by_name(&name,
            drasi_lib::Query::cypher(name.clone())
                .query("MATCH (p:Person) RETURN p.name AS name, false AS updated")
                .from_input_port_by_id(app.output.component.clone(), app.output.port.clone())
                .with_outbox_capacity(16).build()?).await
            .expect_err("failed rebuilding must not complete the accepted operation");
        assert!(error.to_string().contains("bootstrap"), "{error}");
        let pending = app.core.desired_configuration()?.pending_updates;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].component_id, output.id);
        assert!(app.core.component_by_id(&output.id)?.observed()?.failure.is_some());
        assert_eq!(app.source.observed()?.generation, source_generation);
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
            .query_with_provider(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000030e")
                    .expect("fixture UUID"),
                drasi_lib::ComponentName::try_new("names").expect("fixture component name"),
                "MATCH (p:Person) RETURN p.name AS name",
                None,
                None,
                Some(DesiredPipe::Bounded { capacity: 16 }),
            )
            .await?;
        let control = app.core.computation_control()?;
        let generation = app
            .core
            .component_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
            )?
            .generation();
        let edge = EdgeDefinition::new(
            app.output.clone(),
            Endpoint::new(output.id.clone(), PortId::try_new("in")?),
        );
        let error = app
            .core
            .update_query_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name"),
                drasi_lib::Query::cypher(
                    drasi_core::ComponentName::try_new("names").expect("fixture component name"),
                )
                .query("MATCH (p:Person) RETURN p.name AS name, true AS updated")
                .from_input_port_by_id(app.output.component.clone(), app.output.port.clone())
                .build()?,
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
            app.core
                .component_by_name(
                    &drasi_core::ComponentName::try_new("names").expect("fixture component name")
                )?
                .generation(),
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
    let core = lib_integration_tests::query_builder()
        .with_id("native-input-validation")
        .build()
        .await?;
    let result = async {
        let output = Endpoint::new(
            ComponentId::try_new("5d96aa40-8762-53bc-98ef-31742dd40e9c")?,
            PortId::try_new("out")?,
        );
        let bootstrap = Arc::new(ApplicationBootstrapProvider::with_shared_data(Arc::new(
            RwLock::new(Vec::new()),
        )));
        for (bootstrap_id, enabled, source, expected) in [
            (
                component_id(790),
                true,
                output.component.clone(),
                "bootstrap names unknown query",
            ),
            (
                component_id(782),
                false,
                output.component.clone(),
                "requires enabled bootstrap",
            ),
            (
                component_id(782),
                true,
                component_id(791),
                "native input bindings",
            ),
        ] {
            let error = core
                .computation_pipeline()?
                .native_source(output.clone())?
                .query_bootstrap(&bootstrap_id, bootstrap.clone())?
                .query(
                    drasi_lib::Query::cypher(
                        drasi_core::ComponentName::try_new("names")
                            .expect("fixture component name"),
                    )
                    .query("MATCH (p:Person) RETURN p.name AS name")
                    .from_input_by_id(source)
                    .enable_bootstrap(enabled)
                    .build()?
                    .with_assigned_id(component_id(782)),
                )
                .build()
                .err()
                .context("invalid bootstrap declaration was accepted")?;
            assert!(error.to_string().contains(expected), "{error}");
        }
        let mut query = drasi_lib::Query::cypher(
            drasi_core::ComponentName::try_new("names").expect("fixture component name"),
        )
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_input_port_by_id(output.component.clone(), output.port.clone())
        .build()?
        .with_assigned_id(component_id(782));
        query.inputs[0].nodes.push("Person".into());
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
            .native_source(output.clone())?
            .query(
                drasi_lib::Query::cypher(
                    drasi_core::ComponentName::try_new("names").expect("fixture component name"),
                )
                .query("MATCH (p:Person) RETURN p.name AS name")
                .from_input_port_by_id(output.component.clone(), output.port.clone())
                .build()?
                .with_assigned_id(component_id(782)),
            )
            .build()?;
        batch.definition.boundary_relationships[0].pipe = DesiredPipe::Bounded { capacity: 0 };
        assert!(
            core.add_components(batch).await.is_err(),
            "missing sources cannot hide invalid pipes"
        );
        assert!(core
            .component_by_name(
                &drasi_core::ComponentName::try_new("names").expect("fixture component name")
            )
            .is_err());
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
fn native_application_update_resolves_late_factory_primary_and_preserves_resources() -> Result<()> {
    both_runtimes(deferred_native_primary_preserves_resources)
}

#[test]
fn native_application_query_replacement_rejects_custom_inputs_without_mutation() -> Result<()> {
    both_runtimes(custom_inputs_are_not_replaced)
}

#[test]
fn native_application_pipeline_rejects_invalid_bootstrap_selectors_and_pipes() -> Result<()> {
    both_runtimes(native_pipeline_validates_configuration)
}
